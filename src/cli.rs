//! The `perdure` command-line interface — `new`, `check`, `run`, `test`, `fix`,
//! `race`, `trace`, `replay`, `bench`, `audit`. A small hand-rolled dispatcher so
//! the output is exactly what we want, with zero argument-parsing dependencies.

use crate::agent::{self, Strategy};
use crate::ast::Item;
use crate::check::{self, check_program};
use crate::diagnostics::Diagnostic;
use crate::goal::{self, GoalSpec};
use crate::interp::{Interp, Signal};
use crate::patch::Workspace;
use crate::program::Program;
use crate::runner::run_tests;
use crate::trace::{self, TraceFile};
use crate::{
    action, adopt, builtins, event, fmt, guard, mcp, plan, project, render, runtime, schema, store,
    term,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn run(args: Vec<String>) -> i32 {
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => ("help", Vec::new()),
    };
    match cmd {
        "new" => cmd_new(&rest),
        "init" => cmd_init(&rest),
        "guard" => cmd_guard(&rest),
        "check" => cmd_check(&rest),
        "run" => cmd_run(&rest),
        "test" => cmd_test(&rest),
        "fix" => cmd_fix(&rest),
        "fmt" => cmd_fmt(&rest),
        "race" => cmd_race(&rest),
        "trace" => cmd_trace(&rest),
        "replay" => cmd_replay(&rest),
        "bench" => cmd_bench(&rest),
        "audit" => cmd_audit(&rest),
        "goal" => cmd_goal(&rest),
        "serve-mcp" => cmd_serve_mcp(&rest),
        "doctor" => cmd_doctor(&rest),
        "explain" => cmd_explain(&rest),
        "schema" => cmd_schema(&rest),
        "version" | "--version" | "-V" => {
            println!("perdure {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "--help" | "-h" => {
            print_help();
            0
        }
        other => {
            eprintln!("{} unknown command `{}`\n", term::bold_red("error:"), other);
            print_help();
            2
        }
    }
}

// ----- argument parsing -----

struct Parsed {
    pos: Vec<String>,
    flags: HashMap<String, Option<String>>,
}

impl Parsed {
    fn has(&self, k: &str) -> bool {
        self.flags.contains_key(k)
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).and_then(|v| v.as_deref())
    }
}

fn parse(raw: &[String], valued: &[&str]) -> Parsed {
    let mut pos = Vec::new();
    let mut flags = HashMap::new();
    let mut i = 0;
    while i < raw.len() {
        let a = &raw[i];
        if a.starts_with("--") {
            if let Some((k, v)) = a.split_once('=') {
                flags.insert(k.to_string(), Some(v.to_string()));
            } else if valued.contains(&a.as_str()) && i + 1 < raw.len() {
                flags.insert(a.clone(), Some(raw[i + 1].clone()));
                i += 1;
            } else {
                flags.insert(a.clone(), None);
            }
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    Parsed { pos, flags }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn is_file_arg(s: &str) -> bool {
    s.ends_with(".pdr") && Path::new(s).is_file()
}

/// Load a single `.pdr` file if one was named, otherwise the whole project.
fn load_target(p: &Parsed) -> Result<Workspace, String> {
    if let Some(first) = p.pos.iter().find(|s| is_file_arg(s)) {
        return project::load_single(Path::new(first)).map_err(|e| e.to_string());
    }
    project::load_workspace(&cwd()).map_err(|e| e.to_string())
}

/// Parse + check a workspace, returning the program and all diagnostics.
fn analyze(ws: &Workspace) -> (Program, Vec<Diagnostic>) {
    let (prog, mut diags) = ws.program();
    diags.extend(check_program(&prog));
    (prog, diags)
}

fn errors_only(diags: &[Diagnostic]) -> Vec<Diagnostic> {
    diags.iter().filter(|d| d.is_error()).cloned().collect()
}

// ----- commands -----

fn cmd_new(rest: &[String]) -> i32 {
    let p = parse(rest, &["--goal", "--goal-template"]);
    let clean = p.has("--clean");
    let plan_template = p.get("--goal").or_else(|| p.get("--goal-template"));
    let name = p.pos.first().cloned().unwrap_or_else(|| "demo".into());
    match project::scaffold(&cwd(), &name, clean, plan_template) {
        Ok(dir) => {
            println!(
                "  {} created {}",
                term::bold_green("✓"),
                term::bold(&dir.display().to_string())
            );
            println!();
            if let Some(tmpl) = plan_template {
                println!(
                    "  {}",
                    term::dim(&format!(
                        "a workspace-authored plan goal ({tmpl}) — a durable workflow you own."
                    ))
                );
                println!();
                println!("    cd {}", name);
                println!(
                    "    {}  {}",
                    term::bold("perdure goal check ReconcileLocalDemo"),
                    term::dim("# validate the plan")
                );
                println!(
                    "    {}    {}",
                    term::bold("perdure goal run ReconcileLocalDemo"),
                    term::dim("# runs to the first approval gate, then pauses")
                );
                println!(
                    "    {}              {}",
                    term::bold("perdure goal approvals <id>"),
                    term::dim("# grant it, then `perdure goal resume <id>`")
                );
            } else if clean {
                println!("  {}", term::dim("a minimal, green project."));
                println!("  next:  cd {} && perdure run", name);
            } else {
                println!(
                    "  {}",
                    term::dim("a demo with three planted bugs, ready for the repair loop.")
                );
                println!();
                println!("    cd {}", name);
                println!(
                    "    {}   {}",
                    term::bold("perdure check"),
                    term::dim("# the 3 structured diagnostics")
                );
                println!(
                    "    {}     {}",
                    term::bold("perdure fix"),
                    term::dim("# drive it to green in 3 laps")
                );
                println!(
                    "    {}    {}",
                    term::bold("perdure race"),
                    term::dim("# race repair strategies")
                );
                println!(
                    "    {}   {}",
                    term::bold("perdure trace"),
                    term::dim("# inspect the laps")
                );
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_check(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let ws = match load_target(&p) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (_, diags) = analyze(&ws);
    if p.has("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&diags).unwrap_or_default()
        );
        return if diags.iter().any(|d| d.is_error()) {
            1
        } else {
            0
        };
    }
    if diags.is_empty() {
        println!("  {} no problems found", term::bold_green("●"));
        return 0;
    }
    print!("{}", render::diagnostics(&diags, &ws));
    let errs = diags.iter().filter(|d| d.is_error()).count();
    let warns = diags.len() - errs;
    let lead = if errs == 0 {
        term::bold_yellow("●")
    } else {
        term::bold_red("●")
    };
    println!("  {} {} error(s), {} warning(s)", lead, errs, warns);
    if errs > 0 {
        1
    } else {
        0
    }
}

fn cmd_run(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let ws = match load_target(&p) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (prog, diags) = analyze(&ws);
    let errs = errors_only(&diags);
    if !errs.is_empty() {
        print!("{}", render::diagnostics(&errs, &ws));
        println!(
            "  {} cannot run — {} error(s). Try `{}`.",
            term::bold_red("●"),
            errs.len(),
            term::bold("perdure fix")
        );
        return 1;
    }
    let interp = Interp::new(&prog);
    match interp.run_main() {
        Ok(v) => {
            for line in interp.logs() {
                println!("  {} {}", term::dim("log"), line);
            }
            println!("  {} {}", term::bold_green("→"), v);
            0
        }
        Err(sig) => {
            println!(
                "  {} {}",
                term::bold_red("runtime error:"),
                describe_signal(sig)
            );
            1
        }
    }
}

fn cmd_test(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let json = p.has("--json");
    let filter = p.pos.iter().find(|s| !is_file_arg(s)).cloned();
    let ws = match load_target(&p) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (prog, diags) = analyze(&ws);
    let errs = errors_only(&diags);
    if !errs.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({"blocked": true, "errors": errs.len()})
            );
        } else {
            print!("{}", render::diagnostics(&errs, &ws));
            println!(
                "  {} test suite blocked by {} compile error(s). Run `{}`.",
                term::bold_red("●"),
                errs.len(),
                term::bold("perdure fix")
            );
        }
        return 1;
    }
    let report = run_tests(&prog, filter.as_deref());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print!("{}", render::tests(&report));
    }
    if report.all_green() {
        0
    } else {
        1
    }
}

fn cmd_fix(rest: &[String]) -> i32 {
    let p = parse(rest, &["--max-laps", "--strategy", "--coder", "--fixtures"]);
    let dry = p.has("--dry-run");
    let max = p
        .get("--max-laps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let strat = match p.get("--strategy") {
        Some("convert") => Strategy::Convert,
        Some("strict") => Strategy::Strict,
        _ => Strategy::Minimal,
    };
    let root = cwd();
    let ws = match project::load_workspace(&root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    if ws.files.is_empty() {
        println!("  {} no .pdr files found here", term::dim("·"));
        return 1;
    }
    // An optional coder engages only where deterministic repair gives up. The
    // only coder built in is the offline `fixture` one, which replays typed
    // patches from a JSON file through the same verification pipeline.
    let coder = match p.get("--coder") {
        Some("fixture") | None if p.has("--coder") => {
            let path = p.get("--fixtures").unwrap_or(".perdure/fixtures.json");
            match load_fixture_coder(Path::new(path)) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("{} {}: {}", term::bold_red("error:"), path, e);
                    return 1;
                }
            }
        }
        Some(other) => {
            eprintln!(
                "{} unknown coder `{}` (only `fixture` is built in)",
                term::bold_red("error:"),
                other
            );
            return 1;
        }
        None => None,
    };
    let outcome = match &coder {
        Some(c) => agent::fix_with_coder(ws, strat, max, Some(c as &dyn agent::Coder)),
        None => agent::fix(ws, strat, max),
    };
    print!("{}", render::fix(&outcome));
    let _ = trace::save(&root, &TraceFile::Fix(outcome.clone()));

    if dry {
        println!("\n  {} dry run — no files written", term::dim("·"));
    } else if outcome.is_green() {
        match project::write_back(&root, &outcome.base_files, &outcome.final_files) {
            Ok(changed) if !changed.is_empty() => {
                println!("\n  {} updated {}", term::green("✓"), changed.join(", "));
            }
            Ok(_) => {}
            Err(e) => eprintln!("{} {}", term::bold_red("write error:"), e),
        }
    }
    if outcome.is_green() {
        0
    } else {
        1
    }
}

/// `perdure fmt [file] [--check]` — render every `.pdr` file to its one canonical
/// form. With `--check` it writes nothing and exits non-zero if anything would
/// change (the CI gate). Files that don't parse are left untouched.
fn cmd_fmt(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let check = p.has("--check");

    let files: Vec<(String, PathBuf, String)> =
        if let Some(arg) = p.pos.iter().find(|s| s.ends_with(".pdr")) {
            let path = PathBuf::from(arg);
            match std::fs::read_to_string(&path) {
                Ok(t) => vec![(arg.clone(), path, t)],
                Err(e) => {
                    eprintln!("{} {}: {}", term::bold_red("error:"), arg, e);
                    return 1;
                }
            }
        } else {
            let root = cwd();
            match project::load_workspace(&root) {
                Ok(ws) => ws
                    .files
                    .into_iter()
                    .map(|(rel, t)| (rel.clone(), root.join(&rel), t))
                    .collect(),
                Err(e) => {
                    eprintln!("{} {}", term::bold_red("error:"), e);
                    return 1;
                }
            }
        };

    if files.is_empty() {
        println!("  {} no .pdr files found here", term::dim("·"));
        return 0;
    }

    let mut changed: Vec<String> = Vec::new();
    let mut skipped: Vec<(String, &'static str)> = Vec::new();
    for (disp, abs, text) in &files {
        match fmt::format_file(disp, text) {
            Err(fmt::Skip::ParseError) => {
                skipped.push((disp.clone(), "does not parse — run `perdure check`"))
            }
            Err(fmt::Skip::HasComments) => {
                skipped.push((disp.clone(), "has comments (not yet preserved)"))
            }
            Ok(formatted) if &formatted != text => {
                changed.push(disp.clone());
                if !check {
                    if let Err(e) = std::fs::write(abs, &formatted) {
                        eprintln!("{} {}: {}", term::bold_red("write error:"), disp, e);
                        return 1;
                    }
                }
            }
            Ok(_) => {}
        }
    }

    for (s, why) in &skipped {
        println!("  {} skipped {} ({})", term::dim("·"), s, why);
    }

    if check {
        if changed.is_empty() {
            println!(
                "  {} all {} file(s) are formatted",
                term::bold_green("●"),
                files.len()
            );
            0
        } else {
            for f in &changed {
                println!("  {} {}", term::bold_yellow("✗"), f);
            }
            println!(
                "  {} {} file(s) need formatting — run `perdure fmt`",
                term::bold_red("●"),
                changed.len()
            );
            1
        }
    } else {
        if changed.is_empty() {
            println!(
                "  {} already formatted ({} file(s))",
                term::bold_green("●"),
                files.len()
            );
        } else {
            for f in &changed {
                println!("  {} formatted {}", term::green("✓"), f);
            }
        }
        0
    }
}

fn cmd_race(rest: &[String]) -> i32 {
    let p = parse(rest, &["--max-laps"]);
    let max = p
        .get("--max-laps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let apply = p.has("--apply");
    let root = cwd();
    let ws = match project::load_workspace(&root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let outcome = agent::race(ws, max);
    print!("{}", render::race(&outcome));
    let _ = trace::save(&root, &TraceFile::Race(outcome.clone()));

    if apply {
        if let Some(w) = outcome.winner {
            match project::write_back(&root, &outcome.base_files, &outcome.branches[w].final_files)
            {
                Ok(changed) if !changed.is_empty() => {
                    println!(
                        "\n  {} applied winner — updated {}",
                        term::green("✓"),
                        changed.join(", ")
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("{} {}", term::bold_red("write error:"), e),
            }
        }
    }
    if outcome.winner.is_some() {
        0
    } else {
        1
    }
}

fn cmd_trace(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    match trace::load(&cwd()) {
        Some(tf) => {
            if p.has("--json") {
                println!("{}", serde_json::to_string_pretty(&tf).unwrap_or_default());
            } else {
                match tf {
                    TraceFile::Fix(o) => print!("{}", render::fix(&o)),
                    TraceFile::Race(o) => print!("{}", render::race(&o)),
                }
            }
            0
        }
        None => {
            println!(
                "  {} no trace found — run `{}` first",
                term::dim("·"),
                term::bold("perdure fix")
            );
            1
        }
    }
}

fn cmd_replay(_rest: &[String]) -> i32 {
    let root = cwd();
    match trace::load(&root) {
        Some(TraceFile::Fix(prev)) => {
            let mut ws = Workspace::new();
            for (k, v) in &prev.base_files {
                ws.insert(k.clone(), v.clone());
            }
            let strat = match prev.strategy.as_str() {
                "convert" => Strategy::Convert,
                "strict" => Strategy::Strict,
                _ => Strategy::Minimal,
            };
            let again = agent::fix(ws, strat, 64);
            print!("{}", render::fix(&again));
            let identical = again.final_files == prev.final_files
                && again.laps.len() == prev.laps.len()
                && again.status == prev.status;
            if identical {
                println!(
                    "\n  {} replay reproduced the run exactly — {} laps, {}",
                    term::bold_green("●"),
                    again.laps.len(),
                    again.status
                );
                0
            } else {
                println!(
                    "\n  {} replay diverged from the recorded run",
                    term::bold_red("●")
                );
                1
            }
        }
        Some(TraceFile::Race(prev)) => {
            let mut ws = Workspace::new();
            for (k, v) in &prev.base_files {
                ws.insert(k.clone(), v.clone());
            }
            let again = agent::race(ws, 64);
            print!("{}", render::race(&again));
            if again.winner == prev.winner {
                println!(
                    "\n  {} replay reproduced the race — winner unchanged",
                    term::bold_green("●")
                );
                0
            } else {
                println!(
                    "\n  {} replay diverged from the recorded race",
                    term::bold_red("●")
                );
                1
            }
        }
        None => {
            println!("  {} no trace to replay", term::dim("·"));
            1
        }
    }
}

fn cmd_bench(rest: &[String]) -> i32 {
    let p = parse(rest, &["--max-laps", "--suite"]);
    let max = p
        .get("--max-laps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    if p.has("--suite") {
        let dir = p.get("--suite").unwrap_or("corpus");
        return cmd_bench_suite(Path::new(dir), max, p.has("--json"));
    }
    let root = cwd();
    let ws = match project::load_workspace(&root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let outcome = agent::fix(ws, Strategy::Minimal, max);
    let m = &outcome.metrics;
    println!("{}", term::bold("Perdure bench · agent loop"));
    println!(
        "  {}\n",
        term::dim("the metric that matters for AI coding: time from red to green")
    );
    let row = |k: &str, v: String| println!("  {:<20} {}", k, v);
    row("status", outcome.status.clone());
    row("time-to-green", render::fmt_duration(m.us));
    row("laps-to-green", format!("{}", m.laps));
    row("patches-applied", format!("{}", m.patches_applied));
    row("patches-rejected", format!("{}", m.patches_rejected));
    row("tests-run", format!("{}", m.tests_run));
    row("regressions", format!("{}", m.regressions));
    row("diff-size", format!("{} chars", m.diff_chars));
    0
}

/// Build a deterministic fixture coder from a JSON file of typed patches. The
/// schema is a plain array of `Patch` objects (the same shape `perdure check
/// --json` emits per diagnostic), so a model — or a human — can author a fix
/// table the offline loop replays through the verification pipeline.
fn load_fixture_coder(path: &Path) -> Result<agent::FixtureCoder, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let patches: Vec<crate::patch::Patch> =
        serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(agent::FixtureCoder::new(patches))
}

fn cmd_bench_suite(dir: &Path, max: usize, json: bool) -> i32 {
    let cases = match project::load_suite(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} {}: {}", term::bold_red("error:"), dir.display(), e);
            return 1;
        }
    };
    if cases.is_empty() {
        println!(
            "  {} no cases found under {}",
            term::dim("·"),
            dir.display()
        );
        return 1;
    }
    let outcome = agent::run_suite(cases, max);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome).unwrap_or_default()
        );
        return if outcome.all_green() { 0 } else { 1 };
    }
    println!("{}", term::bold("Perdure bench · repair corpus"));
    println!(
        "  {}\n",
        term::dim("red → green over a suite, not just the demo")
    );
    println!(
        "  {:<22} {:>6}  {:>8}  {:>7}  {:>10}",
        "case", "status", "patches", "laps", "time"
    );
    for c in &outcome.cases {
        let m = &c.outcome.metrics;
        let mark = if c.outcome.is_green() {
            term::green("●")
        } else {
            term::bold_red("●")
        };
        println!(
            "  {:<22} {} {:<4}  {:>8}  {:>7}  {:>10}",
            c.name,
            mark,
            c.outcome.status,
            m.patches_applied,
            m.laps,
            render::fmt_duration(m.us),
        );
    }
    let t = &outcome.totals;
    println!();
    let lead = if outcome.all_green() {
        term::green("●")
    } else {
        term::bold_red("●")
    };
    println!(
        "  {} {}/{} green · {} patches · {} tests run · {} regressions · {}",
        lead,
        t.green,
        t.cases,
        t.patches_applied,
        t.tests_run,
        t.regressions,
        render::fmt_duration(t.us),
    );
    if outcome.all_green() {
        0
    } else {
        1
    }
}

fn cmd_audit(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let ws = match load_target(&p) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (prog, _) = ws.program();
    if p.has("--json") {
        let mut arr = Vec::new();
        for u in &prog.units {
            for it in &u.module.items {
                if let Item::Fn(f) = it {
                    let effects: Vec<String> = f
                        .effects
                        .as_ref()
                        .map(|c| c.effects.iter().map(|e| e.name.clone()).collect())
                        .unwrap_or_default();
                    let sensitive: Vec<&String> = effects
                        .iter()
                        .filter(|e| builtins::is_sensitive(e))
                        .collect();
                    arr.push(serde_json::json!({
                        "name": f.name,
                        "signature": check::signature_string(f),
                        "effects": effects,
                        "sensitive": sensitive,
                    }));
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
    } else {
        print!("{}", render::audit(&prog));
    }
    0
}

// ----- goal runtime -----

/// Exit code used when a run stops on a simulated crash (`--crash-after`). It is
/// non-zero (the run did not finish) but distinct from a real failure, so scripts
/// and the demo can tell "crashed, resume me" apart from "the goal failed".
const CRASH_EXIT: i32 = 99;

fn cmd_goal(rest: &[String]) -> i32 {
    let (sub, rest) = match rest.split_first() {
        Some((s, r)) => (s.as_str(), r.to_vec()),
        None => ("", Vec::new()),
    };
    match sub {
        "run" => cmd_goal_run(&rest),
        "init" => cmd_goal_init(&rest),
        "check" => cmd_goal_check(&rest),
        "list" => cmd_goal_list(&rest),
        "inspect" => cmd_goal_inspect(&rest),
        "resume" => cmd_goal_resume(&rest),
        "replay" => cmd_goal_replay(&rest),
        "cancel" => cmd_goal_cancel(&rest),
        "approvals" => cmd_goal_approvals(&rest),
        "approve" => cmd_goal_approve(&rest),
        "deny" => cmd_goal_deny(&rest),
        "receipts" => cmd_goal_receipts(&rest),
        "receipt" => cmd_goal_receipt(&rest),
        "" => cmd_goal_overview(),
        other => {
            eprintln!(
                "{} unknown `perdure goal` subcommand `{}`",
                term::bold_red("error:"),
                other
            );
            print_goal_help();
            2
        }
    }
}

// ----- coding harness: init & guard -----

/// Repo-relative display of a path the adoption wrote (absolute under `root`).
fn show_rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// `perdure init --existing` — adopt the current repo for the coding harness.
fn cmd_init(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    if !p.has("--existing") {
        eprintln!(
            "{} usage: perdure init --existing [--force]",
            term::bold_red("error:")
        );
        eprintln!(
            "  adopts the current repo: writes Perdurefile, PERDURE_AGENT.md, .perdureignore"
        );
        return 2;
    }
    let root = cwd();
    match adopt::init_existing(&root, p.has("--force")) {
        Ok(rep) => {
            match &rep.detected {
                Some(d) => println!(
                    "  {} detected {} project — test command: {}",
                    term::green("✓"),
                    d.ecosystem,
                    term::bold(&rep.command)
                ),
                None => println!(
                    "  {} could not detect a test command — set your real one in the Perdurefile \
                     (`shell.run` + `require command(…)`); `perdure guard begin` refuses the \
                     placeholder",
                    term::bold_yellow("!")
                ),
            }
            for f in &rep.written {
                println!("  {} wrote {}", term::green("+"), show_rel(&root, f));
            }
            for f in &rep.skipped {
                println!(
                    "  {} kept existing {} (use --force to overwrite)",
                    term::dim("·"),
                    show_rel(&root, f)
                );
            }
            println!();
            println!(
                "  next:  {}",
                term::bold(&format!("perdure guard begin {}", rep.goal_name))
            );
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

/// `perdure serve-mcp` — expose Perdure's safe guard/goal operations to an external agent
/// over the MCP stdio transport. Server-only: no raw shell, no arbitrary file writes.
fn cmd_serve_mcp(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    if p.has("--help") || p.has("-h") {
        println!("{}", term::bold("perdure serve-mcp — MCP server (stdio)"));
        println!();
        println!("  Exposes guard/goal operations as MCP tools over stdin/stdout.");
        println!("  Server only: it never runs arbitrary shell or writes files for the agent.");
        println!("  Point your MCP client at:  perdure serve-mcp");
        return 0;
    }
    match mcp::serve(&cwd()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

/// `perdure goal init <template>` — (re)write the Perdurefile for a coding goal.
fn cmd_goal_init(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let id = p
        .pos
        .first()
        .map(|s| s.as_str())
        .unwrap_or("coding.fix-tests");
    let root = cwd();
    match adopt::goal_init(&root, id) {
        Ok((path, name, command)) => {
            println!(
                "  {} wrote {} — goal {} verified by {}",
                term::green("✓"),
                show_rel(&root, &path),
                term::bold(&name),
                term::bold(&command)
            );
            println!(
                "  begin with  {}",
                term::bold(&format!("perdure guard begin {name}"))
            );
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

/// Resolve the run id for a guard subcommand: an explicit positional, else the
/// remembered active session.
fn active_or_pos(p: &Parsed, root: &Path) -> Result<String, i32> {
    if let Some(id) = p.pos.first() {
        return Ok(id.clone());
    }
    match store::active_guard(root) {
        Some(id) => Ok(id),
        None => {
            eprintln!(
                "{} no active guard session — run `perdure guard begin <Goal>`",
                term::bold_red("error:")
            );
            Err(2)
        }
    }
}

fn print_json<T: serde::Serialize>(v: &T) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn cmd_guard(rest: &[String]) -> i32 {
    let (sub, rest) = match rest.split_first() {
        Some((s, r)) => (s.as_str(), r.to_vec()),
        None => ("", Vec::new()),
    };
    match sub {
        "begin" => cmd_guard_begin(&rest),
        "status" => cmd_guard_status(&rest),
        "context" => cmd_guard_context(&rest),
        "next" => cmd_guard_next(&rest),
        "diff" => cmd_guard_diff(&rest),
        "verify" => cmd_guard_verify(&rest),
        // `finalize` is the preferred spelling; `commit` is a back-compat alias.
        // Both are ledger-only and never touch git — the rename exists only to stop
        // the word "commit" reading as a git commit.
        "finalize" | "commit" => cmd_guard_commit(&rest),
        "abort" => cmd_guard_abort(&rest),
        "audit" => cmd_guard_audit(&rest),
        "" => {
            print_guard_help();
            0
        }
        other => {
            eprintln!(
                "{} unknown `perdure guard` subcommand `{}`",
                term::bold_red("error:"),
                other
            );
            print_guard_help();
            2
        }
    }
}

fn cmd_guard_begin(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    match guard::begin(&root, p.pos.first().map(|s| s.as_str())) {
        Ok(state) => {
            let _ = store::set_active_guard(&root, &state.run_id);
            if p.has("--json") {
                if let Ok(ctx) = guard::context(&root, &state.run_id) {
                    print_json(&ctx);
                }
            } else {
                println!(
                    "  {} guard session {} open — goal {}",
                    term::bold_green("●"),
                    term::bold(&state.run_id),
                    term::bold(&state.goal)
                );
                println!(
                    "  edit files in scope, then  {}",
                    term::bold("perdure guard verify")
                );
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

/// `perdure guard audit` — re-derive the run's ledger integrity from outside the agent.
/// Exits non-zero if anything is tampered, so an operator or CI can gate on it.
fn cmd_guard_audit(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    match guard::audit(&root, &id) {
        Ok(r) => {
            if p.has("--json") {
                print_json(&r);
            } else {
                let mark = |ok: bool| {
                    if ok {
                        term::bold_green("✓")
                    } else {
                        term::bold_red("✗")
                    }
                };
                let head = if r.ok {
                    term::bold_green("● ledger intact")
                } else {
                    term::bold_red("● LEDGER TAMPERED")
                };
                println!("  {} — {}", head, term::bold(&r.run_id));
                println!("    {} chain     {}", mark(r.chain_ok), r.chain_detail);
                println!(
                    "    {} receipts  {}",
                    mark(r.receipts_ok),
                    r.receipts_detail
                );
                println!(
                    "    {} verified  {}",
                    mark(r.state_consistent),
                    r.state_detail
                );
            }
            if r.ok {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_status(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    match guard::status(&root, &id) {
        Ok(s) => {
            if p.has("--json") {
                print_json(&s);
            } else {
                println!(
                    "  {} {} — phase {}, verified {}, {}/{} command(s), {} out-of-scope",
                    term::bold(&s.run_id),
                    s.goal,
                    s.phase,
                    s.verified,
                    s.commands_passed,
                    s.commands_required,
                    s.out_of_scope
                );
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_context(rest: &[String]) -> i32 {
    let p = parse(rest, &["--for-agent"]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    // `--for-agent <name>` renders the full, vendor-neutral agent context. Only the
    // stable `generic` shape exists today; provider-specific contexts are deferred.
    if let Some(agent) = p.get("--for-agent") {
        if agent != "generic" {
            eprintln!(
                "{} only `--for-agent generic` is supported (provider-specific contexts are not implemented yet)",
                term::bold_red("error:")
            );
            return 2;
        }
        return match guard::agent_context(&root, &id, agent) {
            Ok(ctx) => {
                print_json(&ctx);
                0
            }
            Err(e) => {
                eprintln!("{} {}", term::bold_red("error:"), e);
                1
            }
        };
    }
    match guard::context(&root, &id) {
        Ok(ctx) => {
            print_json(&ctx);
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_next(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    match guard::next(&root, &id) {
        Ok(n) => {
            if p.has("--json") {
                print_json(&n);
            } else {
                println!(
                    "  {} {} — {}",
                    term::bold(&n.run_id),
                    n.status,
                    term::bold(&n.next_action)
                );
                for line in &n.instructions {
                    println!("    {} {}", term::dim("·"), line);
                }
                if !n.recommended_command.is_empty() {
                    println!("  next:  {}", term::bold(&n.recommended_command));
                }
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_diff(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    match guard::diff(&root, &id) {
        Ok(d) => {
            if p.has("--json") {
                print_json(&d);
            } else {
                for f in &d.in_scope {
                    println!("  {} {}", term::green("✓"), f);
                }
                for f in &d.out_of_scope {
                    println!("  {} {} (out of scope)", term::bold_red("✗"), f);
                }
                if d.added.is_empty() && d.modified.is_empty() && d.deleted.is_empty() {
                    println!("  {} no changes since the baseline", term::dim("·"));
                }
                if !d.blind_spots.is_empty() {
                    println!(
                        "  {} unwatched (frozen): {}",
                        term::dim("·"),
                        term::dim(&d.blind_spots.join(", "))
                    );
                }
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_verify(rest: &[String]) -> i32 {
    let p = parse(rest, &["--crash-after"]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    let rerun = p.has("--rerun");

    // Test/e2e crash hook: `--crash-after receipt:N` stops right after the Nth
    // command receipt is durable but before `verified` is saved, so a resumed
    // verify can be shown to reuse the receipt instead of re-running the command.
    // Exits CRASH_EXIT (the goal-runtime convention) when the crash actually fires.
    if let Some(n) = parse_guard_crash(&p) {
        return match guard::verify_inner(
            &root,
            &id,
            rerun,
            Some(guard::GuardCrash::AfterReceipt(n)),
        ) {
            Ok(s) if !s.verified && s.out_of_scope == 0 && s.receipts >= n => {
                println!(
                    "\n  {} crashed after receipt {} (simulated). State is durable.",
                    term::bold_yellow("✗"),
                    n
                );
                println!(
                    "  resume with  {}",
                    term::bold(&format!("perdure guard verify {}", id))
                );
                CRASH_EXIT
            }
            // Crash point never reached (e.g. the receipt was reused) — fall back to
            // reporting the verify result normally.
            Ok(s) => {
                if p.has("--json") {
                    print_json(&s);
                }
                if s.verified {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                eprintln!("{} {}", term::bold_red("error:"), e);
                1
            }
        };
    }

    match guard::verify_report(&root, &id, rerun) {
        Ok(v) => {
            let s = &v.status;
            if p.has("--json") {
                print_json(&v);
            } else if s.verified {
                println!(
                    "  {} verified — {}/{} command(s) passed, no out-of-scope writes",
                    term::green("✓"),
                    s.commands_passed,
                    s.commands_required
                );
            } else {
                println!(
                    "  {} not verified — {}/{} command(s) passed, {} out-of-scope",
                    term::bold_red("✗"),
                    s.commands_passed,
                    s.commands_required,
                    s.out_of_scope
                );
                // Surface the machine-actionable hint as a human line too.
                if let Some(rej) = &v.rejection {
                    println!("    {} {}", term::dim("·"), rej.message);
                }
                if !v.recommended_command.is_empty() {
                    println!("  next:  {}", term::bold(&v.recommended_command));
                }
            }
            if s.verified {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_commit(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    match guard::commit(&root, &id) {
        Ok(out) => {
            if p.has("--json") {
                print_json(&out);
            } else if out.ok {
                println!(
                    "  {} finalized — run {} is {} (Perdure ledger only; git untouched)",
                    term::green("✓"),
                    out.run_id,
                    out.status
                );
            } else {
                println!(
                    "  {} commit refused — {}",
                    term::bold_red("✗"),
                    out.reason.clone().unwrap_or_default()
                );
            }
            if out.ok {
                store::clear_active_guard(&root);
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_guard_abort(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match active_or_pos(&p, &root) {
        Ok(i) => i,
        Err(c) => return c,
    };
    match guard::abort(&root, &id) {
        Ok(out) => {
            store::clear_active_guard(&root);
            if p.has("--json") {
                print_json(&out);
            } else {
                println!(
                    "  {} aborted — run {} cancelled",
                    term::dim("·"),
                    out.run_id
                );
            }
            0
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn print_guard_help() {
    println!("{}", term::bold("perdure guard — coding-agent session"));
    println!();
    println!("  begin <Goal>     open a session over the working tree");
    println!("  status [--json]  phase, verified bit, command counts");
    println!("  context --json   the operating contract for the agent");
    println!("  context --for-agent generic --json   full agent contract packet");
    println!("  next [--json]    the single next required action for an agent");
    println!("  diff [--json]    changed files, classified by scope");
    println!("  verify [--rerun] run required commands; set the verified bit");
    println!("  finalize         finalize verified changes into Perdure's ledger (never git)");
    println!("  commit           alias for finalize (ledger-only, never git)");
    println!("  abort            cancel the session");
    println!("  audit [--json]   verify the ledger is untampered (chain + receipts + state)");
}

/// `perdure goal` with no subcommand: list the goals declared in this workspace.
fn cmd_goal_overview() -> i32 {
    let ws = match project::load_workspace(&cwd()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (prog, _) = ws.program();
    let goals = goal::all_goals(&prog);
    if !goals.is_empty() {
        println!("{}", term::bold("goals in this workspace"));
        println!();
        for g in goals {
            let spec = GoalSpec::from_decl(g);
            println!(
                "  {}  {}",
                term::bold(&g.name),
                term::dim(&format!(
                    "budget {} steps · {} effect(s) · require [{}]",
                    spec.step_budget(),
                    spec.allow.effects.len(),
                    spec.require.join(", ")
                ))
            );
        }
        println!();
    }

    // Built-in business goals run a fixed action plan and need no workspace.
    println!("{}", term::bold("built-in action goals"));
    println!();
    for name in action::builtin_action_goal_names() {
        if let Some((spec, plan)) = action::builtin_action_goal(name) {
            println!(
                "  {}  {}",
                term::bold(name),
                term::dim(&format!(
                    "{} action(s) · {} tool(s) · budget {} steps",
                    plan.steps.len(),
                    spec.allow.tools.len(),
                    spec.step_budget()
                ))
            );
        }
    }
    println!();

    // Built-in plan goals: durable workflows with control flow (loops, branches,
    // per-iteration approval gates) written in the Perdure plan language.
    println!("{}", term::bold("built-in plan goals"));
    println!();
    for name in plan::builtin_plan_goal_names() {
        if let Some((spec, _)) = plan::builtin_plan_goal(name) {
            println!(
                "  {}  {}",
                term::bold(name),
                term::dim(&format!(
                    "plan workflow · {} tool(s) · budget {} steps",
                    spec.allow.tools.len(),
                    spec.step_budget()
                ))
            );
        }
    }
    println!();
    println!("  run one with  {}", term::bold("perdure goal run <name>"));
    0
}

fn parse_crash_after(p: &Parsed) -> Option<u64> {
    // Accept `--crash-after step:3` or `--crash-after 3`.
    let raw = p.get("--crash-after")?;
    let n = raw.strip_prefix("step:").unwrap_or(raw);
    n.parse().ok()
}

/// The guard analog of `parse_crash_after`: `perdure guard verify --crash-after
/// receipt:N` (bare `N` also accepted) stops right after the Nth command receipt.
fn parse_guard_crash(p: &Parsed) -> Option<usize> {
    let raw = p.get("--crash-after")?;
    let n = raw.strip_prefix("receipt:").unwrap_or(raw);
    n.parse().ok()
}

fn cmd_goal_run(rest: &[String]) -> i32 {
    let p = parse(rest, &["--strategy", "--crash-after"]);
    let name = match p.pos.first() {
        Some(n) => n.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal run <name> [--strategy s] [--crash-after step:N]",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    let strat = strategy_from_flag(p.get("--strategy"));
    let crash_after = parse_crash_after(&p);
    let dry = p.has("--dry-run");
    let root = cwd();

    // A built-in business goal runs a fixed action plan — it needs no workspace.
    if let Some((spec, plan)) = action::builtin_action_goal(&name) {
        let fp = store::fingerprint(&spec.name, &std::collections::BTreeMap::new());
        let prior = store::runs_for_fingerprint(&root, &fp);
        if !prior.is_empty() {
            println!(
                "  {} {} prior run(s) of this goal exist; starting a new one (histories are kept)",
                term::dim("·"),
                prior.len()
            );
        }
        let crash = crash_after.map(runtime::ActionCrash::Step);
        let result = match runtime::start_action_run(&root, spec, plan, crash) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} {}", term::bold_red("error:"), e);
                return 1;
            }
        };
        return finish_run(&root, result, dry);
    }

    // A built-in plan goal runs a durable workflow (loops/branches/approvals) and
    // likewise needs no workspace — the plan body is the program.
    if let Some((spec, plan_block)) = plan::builtin_plan_goal(&name) {
        let fp = store::fingerprint(&spec.name, &std::collections::BTreeMap::new());
        let prior = store::runs_for_fingerprint(&root, &fp);
        if !prior.is_empty() {
            println!(
                "  {} {} prior run(s) of this goal exist; starting a new one (histories are kept)",
                term::dim("·"),
                prior.len()
            );
        }
        let crash = crash_after.map(runtime::ActionCrash::Step);
        // A built-in goal carries no source snapshot; the plan is re-derived from
        // the catalog on resume.
        let result = match runtime::start_plan_run(
            &root,
            spec,
            plan_block,
            std::collections::BTreeMap::new(),
            crash,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} {}", term::bold_red("error:"), e);
                return 1;
            }
        };
        return finish_run(&root, result, dry);
    }

    let ws = match project::load_workspace(&root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (prog, _) = ws.program();
    let decl = match goal::find_goal(&prog, &name) {
        Some(g) => g,
        None => {
            eprintln!(
                "{} no goal named `{}` in this workspace",
                term::bold_red("error:"),
                name
            );
            let names: Vec<String> = goal::all_goals(&prog)
                .iter()
                .map(|g| g.name.clone())
                .collect();
            if !names.is_empty() {
                eprintln!("  available goals: {}", names.join(", "));
            }
            return 1;
        }
    };
    let spec = GoalSpec::from_decl(decl);
    // A goal that carries a `plan { … }` block is a durable workflow, not a repair
    // run. It needs no source to *fix*, but we snapshot the workspace so resume and
    // replay re-parse the frozen plan rather than the live (possibly edited) file.
    let plan_block = decl.plan.clone();

    // A fresh run never overwrites a prior one. If earlier runs of this goal over
    // this source exist, say so — and point at resume in case one was unfinished.
    let fp = store::fingerprint(&spec.name, &ws.files);
    let prior = store::runs_for_fingerprint(&root, &fp);
    if !prior.is_empty() {
        println!(
            "  {} {} prior run(s) of this goal exist; starting a new one (histories are kept)",
            term::dim("·"),
            prior.len()
        );
        for id in &prior {
            if let Ok(s) = store::load_state(&root, id) {
                if matches!(s.status.as_str(), "running" | "budget_exhausted") {
                    println!(
                        "    {} `{}` is {} — resume it with `perdure goal resume {}`",
                        term::dim("·"),
                        id,
                        s.status,
                        id
                    );
                }
            }
        }
    }

    if let Some(plan_block) = plan_block {
        let crash = crash_after.map(runtime::ActionCrash::Step);
        let result = match runtime::start_plan_run(&root, spec, plan_block, ws.files.clone(), crash)
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{} {}", term::bold_red("error:"), e);
                return 1;
            }
        };
        return finish_run(&root, result, dry);
    }

    let result = match runtime::start_run(&root, spec, ws, strat, crash_after) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    finish_run(&root, result, dry)
}

fn cmd_goal_resume(rest: &[String]) -> i32 {
    let p = parse(rest, &["--crash-after"]);
    let id = match p.pos.first() {
        Some(i) => i.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal resume <run-id>",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    let crash_after = parse_crash_after(&p);
    let dry = p.has("--dry-run");
    let root = cwd();

    let state = match store::load_state(&root, &id) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "{} no run `{}` in the goal store",
                term::bold_red("error:"),
                id
            );
            return 1;
        }
    };
    if matches!(state.status.as_str(), "completed" | "cancelled" | "denied") {
        println!(
            "  {} run `{}` is already {} — nothing to resume",
            term::dim("·"),
            id,
            state.status
        );
        return 0;
    }

    let result = match runtime::resume_run(&root, &id, crash_after) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    finish_run(&root, result, dry)
}

/// `perdure goal check <name>` — statically validate a workspace goal (its plan, if it
/// has one) before any run. A focused view of the same checks `perdure check` runs.
fn cmd_goal_check(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let name = match p.pos.first() {
        Some(n) => n.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal check <name>",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    let ws = match project::load_workspace(&cwd()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let (prog, _) = ws.program();
    let diags = match check::check_named_goal(&prog, &name) {
        Some(d) => d,
        None => {
            // Not authored in this workspace. Built-in goals ship verified.
            if action::builtin_action_goal(&name).is_some()
                || plan::builtin_plan_goal(&name).is_some()
            {
                println!(
                    "  {} `{}` is a built-in goal — its plan ships verified",
                    term::bold_green("●"),
                    name
                );
                return 0;
            }
            eprintln!(
                "{} no goal named `{}` in this workspace",
                term::bold_red("error:"),
                name
            );
            let names: Vec<String> = goal::all_goals(&prog)
                .iter()
                .map(|g| g.name.clone())
                .collect();
            if !names.is_empty() {
                eprintln!("  available goals: {}", names.join(", "));
            }
            return 1;
        }
    };
    if p.has("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&diags).unwrap_or_default()
        );
        return if diags.iter().any(|d| d.is_error()) {
            1
        } else {
            0
        };
    }
    if diags.is_empty() {
        println!("  {} goal `{}` checks out", term::bold_green("●"), name);
        return 0;
    }
    print!("{}", render::diagnostics(&diags, &ws));
    let errs = diags.iter().filter(|d| d.is_error()).count();
    let warns = diags.len() - errs;
    let lead = if errs == 0 {
        term::bold_yellow("●")
    } else {
        term::bold_red("●")
    };
    println!("  {} {} error(s), {} warning(s)", lead, errs, warns);
    if errs > 0 {
        1
    } else {
        0
    }
}

/// Shared tail for `run`/`resume`: print the outcome, write verified files back on
/// success, and choose an honest exit code.
fn finish_run(root: &Path, result: runtime::RunResult, dry: bool) -> i32 {
    let st = &result.state;
    print!("{}", render::run_state(st));

    if result.crashed {
        println!(
            "\n  {} crashed after step {} (simulated). State is durable.",
            term::bold_yellow("✗"),
            st.step
        );
        println!(
            "  resume with  {}",
            term::bold(&format!("perdure goal resume {}", st.run_id))
        );
        return CRASH_EXIT;
    }

    if result.paused {
        println!(
            "\n  {} awaiting approval — review with {}",
            term::bold_yellow("⏸"),
            term::bold(&format!("perdure goal approvals {}", st.run_id))
        );
        return 0;
    }

    // Action and plan goals have no workspace to write back; they prove their
    // work with receipts.
    if st.kind == "action" || st.kind == "plan" {
        return match st.status.as_str() {
            "completed" => {
                println!(
                    "\n  {} goal completed — {} receipt(s). See {}",
                    term::green("✓"),
                    st.receipts_created,
                    term::bold(&format!("perdure goal receipts {}", st.run_id))
                );
                0
            }
            _ => 1,
        };
    }

    match st.status.as_str() {
        "completed" => {
            if dry {
                println!("\n  {} dry run — no files written", term::dim("·"));
            } else if let Ok(record) = store::load_goal(root, &st.run_id) {
                match project::write_back(root, &record.base_files, &result.final_files) {
                    Ok(changed) if !changed.is_empty() => {
                        println!("\n  {} updated {}", term::green("✓"), changed.join(", "));
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("{} {}", term::bold_red("write error:"), e),
                }
            }
            0
        }
        _ => 1,
    }
}

fn cmd_goal_list(_rest: &[String]) -> i32 {
    let root = cwd();
    let ids = store::list_runs(&root);
    if ids.is_empty() {
        println!(
            "  {} no goal runs yet — start one with `perdure goal run <name>`",
            term::dim("·")
        );
        return 0;
    }
    println!("{}", term::bold("goal runs"));
    println!();
    let (h_run, h_status, h_step, h_goal) = ("run", "status", "step", "goal");
    println!("  {:<22} {:<14} {:>5}  {}", h_run, h_status, h_step, h_goal);
    for id in &ids {
        if let Ok(s) = store::load_state(&root, id) {
            println!(
                "  {:<22} {} {:<12} {:>5}  {}",
                id,
                render::status_dot(&s.status),
                s.status,
                s.step,
                s.goal
            );
        }
    }
    0
}

fn cmd_goal_inspect(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match p.pos.first() {
        Some(i) => i.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal inspect <run-id>",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    let state = match store::load_state(&root, &id) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "{} no run `{}` in the goal store",
                term::bold_red("error:"),
                id
            );
            return 1;
        }
    };
    let events = event::read_all(&store::events_path(&root, &id)).unwrap_or_default();
    if p.has("--json") {
        let record = store::load_goal(&root, &id).ok();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "goal": record.map(|r| r.spec),
                "state": state,
                "events": events,
            }))
            .unwrap_or_default()
        );
        return 0;
    }
    print!("{}", render::run_state(&state));
    print!("{}", render::events(&events));
    0
}

fn cmd_goal_replay(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match p.pos.first() {
        Some(i) => i.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal replay <run-id> [--rerun]",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    // For a coding run, default replay re-derives the verdict from the recorded
    // receipts (no command runs); `--rerun` actually re-executes the commands.
    let rerun = p.has("--rerun");
    match runtime::replay_run(&root, &id, rerun) {
        Ok(r) => {
            if r.identical {
                println!(
                    "  {} replay reproduced the run exactly — {} step(s), {}",
                    term::bold_green("●"),
                    r.steps,
                    r.replayed_status
                );
                0
            } else {
                println!(
                    "  {} replay diverged — recorded `{}`, replayed `{}`",
                    term::bold_red("●"),
                    r.recorded_status,
                    r.replayed_status
                );
                1
            }
        }
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            1
        }
    }
}

fn cmd_goal_cancel(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match p.pos.first() {
        Some(i) => i.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal cancel <run-id>",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    match runtime::cancel_run(&root, &id) {
        Ok(s) => {
            println!("  {} run `{}` is now {}", term::green("✓"), id, s.status);
            0
        }
        Err(_) => {
            eprintln!(
                "{} no run `{}` in the goal store",
                term::bold_red("error:"),
                id
            );
            1
        }
    }
}

// ----- action layer: approvals & receipts -----

/// Read a `<run-id> <id>` pair of positionals, or print a usage error.
fn two_positionals<'a>(p: &'a Parsed, usage: &str) -> Option<(&'a str, &'a str)> {
    match (p.pos.first(), p.pos.get(1)) {
        (Some(a), Some(b)) => Some((a.as_str(), b.as_str())),
        _ => {
            eprintln!("{} usage: {}", term::bold_red("error:"), usage);
            None
        }
    }
}

fn cmd_goal_approvals(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match p.pos.first() {
        Some(i) => i.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal approvals <run-id>",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    if store::load_state(&root, &id).is_err() {
        eprintln!(
            "{} no run `{}` in the goal store",
            term::bold_red("error:"),
            id
        );
        return 1;
    }
    let items = store::list_approvals(&root, &id);
    if items.is_empty() {
        println!(
            "  {} no approvals recorded for run `{}`",
            term::dim("·"),
            id
        );
        return 0;
    }
    print!("{}", render::approvals(&items));
    0
}

fn cmd_goal_approve(rest: &[String]) -> i32 {
    decide_approval(rest, "granted")
}

fn cmd_goal_deny(rest: &[String]) -> i32 {
    decide_approval(rest, "denied")
}

/// Shared body for `approve`/`deny`: flip the approval file to a terminal status and
/// append the decision event. The decision is recorded once, here, by the human's
/// command — the runtime never re-emits it on resume. An already-decided approval is
/// terminal: a second decision is refused.
fn decide_approval(rest: &[String], decision: &str) -> i32 {
    let p = parse(rest, &["--note", "--reason"]);
    let granting = decision == "granted";
    let usage = if granting {
        "perdure goal approve <run-id> <approval-id> [--note ...]"
    } else {
        "perdure goal deny <run-id> <approval-id> [--reason ...]"
    };
    let (id, apr_id) = match two_positionals(&p, usage) {
        Some(v) => v,
        None => return 2,
    };
    let root = cwd();
    let mut approval = match store::load_approval(&root, id, apr_id) {
        Ok(a) => a,
        Err(_) => {
            eprintln!(
                "{} no approval `{}` for run `{}`",
                term::bold_red("error:"),
                apr_id,
                id
            );
            return 1;
        }
    };
    if approval.status != "pending" {
        eprintln!(
            "{} approval `{}` is already {} — decisions are final",
            term::bold_red("error:"),
            apr_id,
            approval.status
        );
        return 1;
    }
    approval.status = decision.to_string();
    approval.note = p
        .get("--note")
        .or_else(|| p.get("--reason"))
        .map(str::to_string);
    if let Err(e) = store::save_approval(&root, id, &approval) {
        eprintln!("{} {}", term::bold_red("error:"), e);
        return 1;
    }
    let mut log = match event::EventLog::resume(&store::events_path(&root, id), id) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{} {}", term::bold_red("error:"), e);
            return 1;
        }
    };
    let kind = if granting {
        event::kind::APPROVAL_GRANTED
    } else {
        event::kind::APPROVAL_DENIED
    };
    let _ = log.append(
        kind,
        serde_json::json!({
            "approval_id": approval.id,
            "action": approval.action_id,
            "tool": approval.tool,
            "note": approval.note,
        }),
    );
    if granting {
        println!(
            "  {} approved `{}` — resume with {}",
            term::green("✓"),
            apr_id,
            term::bold(&format!("perdure goal resume {}", id))
        );
    } else {
        println!(
            "  {} denied `{}` — the action will be skipped on resume",
            term::bold_yellow("✗"),
            apr_id
        );
    }
    0
}

fn cmd_goal_receipts(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let root = cwd();
    let id = match p.pos.first() {
        Some(i) => i.clone(),
        None => {
            eprintln!(
                "{} usage: perdure goal receipts <run-id>",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    if store::load_state(&root, &id).is_err() {
        eprintln!(
            "{} no run `{}` in the goal store",
            term::bold_red("error:"),
            id
        );
        return 1;
    }
    let items = store::list_receipts(&root, &id);
    if items.is_empty() {
        println!("  {} no receipts recorded for run `{}`", term::dim("·"), id);
        return 0;
    }
    print!("{}", render::receipts(&items));
    0
}

fn cmd_goal_receipt(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let (id, rid) = match two_positionals(&p, "perdure goal receipt <run-id> <receipt-id> [--json]")
    {
        Some(v) => v,
        None => return 2,
    };
    let root = cwd();
    let receipt = match store::load_receipt(&root, id, rid) {
        Ok(r) => r,
        Err(_) => {
            eprintln!(
                "{} no receipt `{}` for run `{}`",
                term::bold_red("error:"),
                rid,
                id
            );
            return 1;
        }
    };
    if p.has("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt).unwrap_or_default()
        );
    } else {
        print!("{}", render::receipt(&receipt));
    }
    0
}

fn strategy_from_flag(flag: Option<&str>) -> Strategy {
    match flag {
        Some("convert") => Strategy::Convert,
        Some("strict") => Strategy::Strict,
        _ => Strategy::Minimal,
    }
}

fn print_goal_help() {
    let b = |s: &str| term::bold(s);
    println!();
    println!("{}", b("GOAL SUBCOMMANDS"));
    let cmd = |name: &str, desc: &str| println!("  {:<28} {}", name, term::dim(desc));
    cmd(
        "goal run <name>",
        "start a durable run (--strategy, --crash-after step:N)",
    );
    cmd(
        "goal init <template>",
        "write a Perdurefile coding goal (e.g. coding.fix-tests)",
    );
    cmd(
        "goal check <name>",
        "statically validate a goal's plan before running it (--json)",
    );
    cmd("goal list", "list runs in the store");
    cmd(
        "goal inspect <id>",
        "show a run's state + event history (--json)",
    );
    cmd(
        "goal resume <id>",
        "resume a crashed/incomplete run from its last checkpoint",
    );
    cmd(
        "goal replay <id>",
        "re-run from base and prove it reproduces",
    );
    cmd("goal cancel <id>", "cancel a run");
    println!();
    cmd("goal approvals <id>", "list a run's approval gates");
    cmd(
        "goal approve <id> <apr>",
        "grant a pending approval (--note)",
    );
    cmd("goal deny <id> <apr>", "deny a pending approval (--reason)");
    cmd("goal receipts <id>", "list a run's effect receipts");
    cmd("goal receipt <id> <rcpt>", "show one receipt (--json)");
}

// ----- doctor / explain / schema -----

fn cmd_doctor(_rest: &[String]) -> i32 {
    let root = cwd();
    println!("{}", term::bold("perdure doctor"));
    println!(
        "  {}",
        term::dim("a hermetic health check — no network, no clock, no services")
    );
    println!();

    let mut problems = 0;
    let ok = |label: &str, detail: String| {
        println!(
            "  {} {:<22} {}",
            term::green("✓"),
            label,
            term::dim(&detail)
        );
    };
    let warn = |label: &str, detail: String| {
        println!("  {} {:<22} {}", term::bold_yellow("!"), label, detail);
    };

    ok("version", format!("perdure {}", env!("CARGO_PKG_VERSION")));
    ok(
        "determinism",
        "fixed clock, no randomness, byte-exact replay".into(),
    );

    match project::load_workspace(&root) {
        Ok(ws) if ws.files.is_empty() => {
            warn("workspace", "no .pdr files found here".into());
        }
        Ok(ws) => {
            ok("workspace", format!("{} .pdr file(s)", ws.files.len()));
            let (prog, pdiags) = ws.program();
            let errs = pdiags.iter().filter(|d| d.is_error()).count()
                + check_program(&prog).iter().filter(|d| d.is_error()).count();
            if errs == 0 {
                ok("check", "no errors".into());
            } else {
                warn("check", format!("{} error(s) — run `perdure check`", errs));
                problems += 1;
            }
            let goals = goal::all_goals(&prog).len();
            ok("goals", format!("{} declared", goals));
        }
        Err(e) => {
            warn("workspace", format!("could not load: {}", e));
            problems += 1;
        }
    }

    // The store directory must be writable for goal runs to be durable.
    let perdure_dir = root.join(".perdure");
    match std::fs::create_dir_all(&perdure_dir) {
        Ok(_) => ok("store", ".perdure is writable".into()),
        Err(e) => {
            warn("store", format!(".perdure not writable: {}", e));
            problems += 1;
        }
    }

    let runs = store::list_runs(&root).len();
    ok("goal runs", format!("{} in the store", runs));

    println!();
    if problems == 0 {
        println!("  {} everything looks healthy", term::bold_green("●"));
        0
    } else {
        println!(
            "  {} {} thing(s) need attention",
            term::bold_yellow("●"),
            problems
        );
        1
    }
}

fn cmd_explain(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    let code = match p.pos.first() {
        Some(c) => c.to_uppercase(),
        None => {
            eprintln!(
                "{} usage: perdure explain <diagnostic-code>  e.g. perdure explain E0421",
                term::bold_red("error:")
            );
            return 2;
        }
    };
    match explanation(&code) {
        Some((title, body)) => {
            println!("{}  {}", term::bold(&code), term::bold(title));
            println!();
            for line in body.lines() {
                println!("  {}", line);
            }
            0
        }
        None => {
            eprintln!("{} no explanation for `{}`", term::bold_red("error:"), code);
            eprintln!("  known codes: {}", EXPLAINED_CODES.join(", "));
            1
        }
    }
}

const EXPLAINED_CODES: &[&str] = &["E0309", "E0322", "E0421", "E0431", "E0432"];

/// Long-form explanations for the diagnostics an agent (or a human) most often
/// meets. Each pairs the *why* with the repair the compiler already proposes.
fn explanation(code: &str) -> Option<(&'static str, &'static str)> {
    Some(match code {
        "E0309" => (
            "type_mismatch",
            "An expression's type does not match the type required in its position —\n\
             for example a function annotated `-> String` whose body returns an `Int`.\n\n\
             The diagnostic carries a `preferred_patch` that repairs the smaller side:\n\
             usually the annotation. The `convert` repair strategy instead wraps the\n\
             value (e.g. `to_string(x)`), which `perdure race` can weigh against the minimal fix.",
        ),
        "E0322" => (
            "unknown_module",
            "A builtin module (`db`, `time`, `log`, `net`, `math`) is used in this file\n\
             but never imported. Every builtin a body touches must be imported in that file.\n\n\
             The preferred patch inserts the missing `import` at the top of the file.",
        ),
        "E0421" => (
            "effect_undeclared",
            "A function performs an effect (such as `db.read` or `log.write`) that its\n\
             signature does not declare. Effects are part of a function's contract: the\n\
             checker reconciles what a body *does* with what it *says*.\n\n\
             The preferred patch rewrites the `effects [...]` clause to the union of the\n\
             declared and performed effects — or inserts a fresh clause if none exists.\n\
             In a goal run, only effects the goal's `allow` block grants may appear here.",
        ),
        "E0431" => (
            "unknown_require_condition",
            "A goal's `require { ... }` block names a success condition the runtime cannot\n\
             evaluate, so the goal could never be satisfied.\n\n\
             Use one of: tests.pass, no_new_effects, no_forbidden_effects, check.clean.",
        ),
        "E0432" => (
            "goal_unbounded",
            "A goal declares no `steps` or `retries` budget, so a run could loop without a\n\
             bound. Long-horizon runs must be bounded.\n\n\
             Add a `budget { steps: N }` block (and optionally `retries: N`).",
        ),
        _ => return None,
    })
}

fn cmd_schema(rest: &[String]) -> i32 {
    let p = parse(rest, &[]);
    match p.pos.first() {
        None => {
            println!("{}", term::bold("published schemas"));
            println!();
            for s in schema::SCHEMAS {
                println!("  {:<12} {}", term::bold(s.name), term::dim(s.title));
            }
            println!();
            println!("  print one with  {}", term::bold("perdure schema <name>"));
            0
        }
        Some(name) => match schema::get(name) {
            Some(s) => {
                println!("{}", s.json);
                0
            }
            None => {
                eprintln!("{} no schema named `{}`", term::bold_red("error:"), name);
                let names: Vec<&str> = schema::SCHEMAS.iter().map(|s| s.name).collect();
                eprintln!("  available: {}", names.join(", "));
                1
            }
        },
    }
}

fn describe_signal(s: Signal) -> String {
    match s {
        Signal::Error(m, _) => m,
        Signal::Ensure(i) => format!("ensure failed: {}", i.text),
        Signal::Propagate(v) => format!("uncaught {}", v),
        Signal::Return(_) => "returned outside a function".into(),
    }
}

fn print_help() {
    let b = |s: &str| term::bold(s);
    println!(
        "{}",
        term::bold("perdure — a typed goal runtime for long-horizon agents")
    );
    println!(
        "{}",
        term::dim("the fastest path from a failing goal to a verified result")
    );
    println!();
    println!("{}", b("USAGE"));
    println!("  perdure <command> [args]");
    println!();
    println!("{}", b("COMMANDS"));
    let cmd = |name: &str, desc: &str| println!("  {:<16} {}", name, term::dim(desc));
    cmd(
        "new <name>",
        "scaffold a project (--clean for an empty one)",
    );
    cmd(
        "init --existing",
        "adopt an existing repo: write Perdurefile, PERDURE_AGENT.md, .perdureignore",
    );
    cmd(
        "guard <sub>",
        "coding-agent session: begin, status, context, next, diff, verify, finalize, abort",
    );
    cmd(
        "check [file]",
        "type- and effect-check; --json for the machine view",
    );
    cmd("run [file]", "run the project's `main`");
    cmd(
        "test [filter]",
        "run tests (blocked while the project has errors)",
    );
    cmd(
        "fix",
        "run the agentic repair loop to green (--strategy, --dry-run, --coder fixture)",
    );
    cmd("fmt [file]", "format to the one canonical style (--check)");
    cmd(
        "race",
        "race repair strategies in isolation; --apply the winner",
    );
    cmd("trace", "show the last fix/race run (--json)");
    cmd("replay", "re-run the last loop and prove it reproduces");
    cmd(
        "bench",
        "report agent-loop metrics (time-to-green, laps, ...); --suite <dir>",
    );
    cmd("audit [file]", "show every function's effect surface");
    cmd(
        "goal <sub>",
        "the durable goal runtime: run, list, inspect, resume, replay, cancel",
    );
    cmd(
        "serve-mcp",
        "expose guard/goal operations to an external agent over MCP (stdio, server-only)",
    );
    cmd(
        "doctor",
        "hermetic health check of the toolchain + workspace",
    );
    cmd(
        "explain <code>",
        "long-form explanation of a diagnostic code",
    );
    cmd(
        "schema [name]",
        "print a versioned JSON schema for machine output",
    );
    cmd("version", "print the version");
    println!();
    println!("{}", b("EXAMPLE"));
    println!("  perdure new demo && cd demo");
    println!(
        "  perdure check        {}",
        term::dim("# 3 structured diagnostics")
    );
    println!("  perdure fix          {}", term::dim("# green in 3 laps"));
    println!();
    println!("{}", b("GOAL RUNTIME"));
    println!(
        "  perdure goal run FixFailingTests --crash-after step:2   {}",
        term::dim("# durable, crashes mid-run")
    );
    println!(
        "  perdure goal resume <id>                                {}",
        term::dim("# picks up — no repeated work")
    );
    println!(
        "  perdure goal inspect <id>                               {}",
        term::dim("# state + event history")
    );
}
