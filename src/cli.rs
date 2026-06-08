//! The `tach` command-line interface — `new`, `check`, `run`, `test`, `fix`,
//! `race`, `trace`, `replay`, `bench`, `audit`. A small hand-rolled dispatcher so
//! the output is exactly what we want, with zero argument-parsing dependencies.

use crate::agent::{self, Strategy};
use crate::ast::Item;
use crate::check::{self, check_program};
use crate::diagnostics::Diagnostic;
use crate::interp::{Interp, Signal};
use crate::patch::Workspace;
use crate::program::Program;
use crate::runner::run_tests;
use crate::trace::{self, TraceFile};
use crate::{builtins, project, render, term};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn run(args: Vec<String>) -> i32 {
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => ("help", Vec::new()),
    };
    match cmd {
        "new" => cmd_new(&rest),
        "check" => cmd_check(&rest),
        "run" => cmd_run(&rest),
        "test" => cmd_test(&rest),
        "fix" => cmd_fix(&rest),
        "race" => cmd_race(&rest),
        "trace" => cmd_trace(&rest),
        "replay" => cmd_replay(&rest),
        "bench" => cmd_bench(&rest),
        "audit" => cmd_audit(&rest),
        "version" | "--version" | "-V" => {
            println!("tach {}", env!("CARGO_PKG_VERSION"));
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
    s.ends_with(".tach") && Path::new(s).is_file()
}

/// Load a single `.tach` file if one was named, otherwise the whole project.
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
    let p = parse(rest, &[]);
    let clean = p.has("--clean");
    let name = p.pos.first().cloned().unwrap_or_else(|| "demo".into());
    match project::scaffold(&cwd(), &name, clean) {
        Ok(dir) => {
            println!(
                "  {} created {}",
                term::bold_green("✓"),
                term::bold(&dir.display().to_string())
            );
            println!();
            if clean {
                println!("  {}", term::dim("a minimal, green project."));
                println!("  next:  cd {} && tach run", name);
            } else {
                println!(
                    "  {}",
                    term::dim("a demo with three planted bugs, ready for the repair loop.")
                );
                println!();
                println!("    cd {}", name);
                println!(
                    "    {}   {}",
                    term::bold("tach check"),
                    term::dim("# the 3 structured diagnostics")
                );
                println!(
                    "    {}     {}",
                    term::bold("tach fix"),
                    term::dim("# drive it to green in 3 laps")
                );
                println!(
                    "    {}    {}",
                    term::bold("tach race"),
                    term::dim("# race repair strategies")
                );
                println!(
                    "    {}   {}",
                    term::bold("tach trace"),
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
            term::bold("tach fix")
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
                term::bold("tach fix")
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
    let p = parse(rest, &["--max-laps", "--strategy"]);
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
        println!("  {} no .tach files found here", term::dim("·"));
        return 1;
    }
    let outcome = agent::fix(ws, strat, max);
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
                term::bold("tach fix")
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
    let p = parse(rest, &["--max-laps"]);
    let max = p
        .get("--max-laps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
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
    println!("{}", term::bold("Tach bench · agent loop"));
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
        term::bold("tach — the fast language for coding agents")
    );
    println!("{}", term::dim("prompt-to-passing-tests at compiled speed"));
    println!();
    println!("{}", b("USAGE"));
    println!("  tach <command> [args]");
    println!();
    println!("{}", b("COMMANDS"));
    let cmd = |name: &str, desc: &str| println!("  {:<16} {}", name, term::dim(desc));
    cmd(
        "new <name>",
        "scaffold a project (--clean for an empty one)",
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
        "run the agentic repair loop to green (--strategy, --dry-run)",
    );
    cmd(
        "race",
        "race repair strategies in isolation; --apply the winner",
    );
    cmd("trace", "show the last fix/race run (--json)");
    cmd("replay", "re-run the last loop and prove it reproduces");
    cmd(
        "bench",
        "report agent-loop metrics (time-to-green, laps, ...)",
    );
    cmd("audit [file]", "show every function's effect surface");
    cmd("version", "print the version");
    println!();
    println!("{}", b("EXAMPLE"));
    println!("  tach new demo && cd demo");
    println!(
        "  tach check        {}",
        term::dim("# 3 structured diagnostics")
    );
    println!("  tach fix          {}", term::dim("# green in 3 laps"));
}
