//! The Perdure checker: type and effect analysis.
//!
//! Every error it produces is *agent-shaped*. It does not merely say "this is
//! wrong" — it attaches a `preferred_patch` (a byte-span replacement) that, when
//! applied, fixes the problem. A repair agent never has to guess where or how to
//! edit; it reads the patch off the diagnostic and applies it.

use crate::ast::*;
use crate::builtins;
use crate::diagnostics::{Diagnostic, PreferredPatch};
use crate::program::{Program, Unit};
use crate::span::Span;
use crate::types::{compatible, type_from_ast, Type, TypeRegistry};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The declared surface of a function: its effects and return type.
struct FnSig {
    effects: BTreeSet<String>,
    ret: Option<Type>,
}

/// What a body actually does, gathered by walking it.
#[derive(Default)]
struct Usage {
    /// First use site of each builtin module referenced in the body.
    module_uses: BTreeMap<String, Span>,
    /// The set of effects the body actually performs.
    effects: BTreeSet<String>,
}

/// Check a whole program, returning every diagnostic (errors and warnings).
pub fn check_program(program: &Program) -> Vec<Diagnostic> {
    let reg = program.type_registry();
    let sigs = build_sigs(program);
    let mut diags = Vec::new();

    for unit in &program.units {
        // --- import check: every builtin module used anywhere in the file must
        // be imported. Gathered once per file and deduped to the first use site.
        let mut file_modules: BTreeMap<String, Span> = BTreeMap::new();
        for item in &unit.module.items {
            let body = match item {
                Item::Fn(f) => Some(&f.body),
                Item::Test(t) => Some(&t.body),
                _ => None,
            };
            if let Some(b) = body {
                let usage = analyze_block(b, &sigs);
                for (m, sp) in usage.module_uses {
                    file_modules.entry(m).or_insert(sp);
                }
            }
        }
        for (module, span) in &file_modules {
            if !unit.imports.contains(module) {
                diags.push(unknown_module_diag(unit, module, *span));
            }
        }

        // --- unused imports: a module imported but never referenced by any body
        // in this file. A lint, not an error.
        for item in &unit.module.items {
            if let Item::Import(im) = item {
                if !file_modules.contains_key(&im.module) {
                    diags.push(unused_import_diag(unit, im));
                }
            }
        }

        // --- per-function effect + type checks, plus field-access checks over
        // every body (functions and tests).
        for item in &unit.module.items {
            match item {
                Item::Fn(f) => {
                    check_fn_effects(f, unit, &sigs, &mut diags);
                    check_fn_types(f, unit, &reg, &sigs, &mut diags);
                    let mut env: HashMap<String, Type> = f
                        .params
                        .iter()
                        .map(|p| (p.name.clone(), type_from_ast(&p.ty)))
                        .collect();
                    check_block_fields(&f.body, &mut env, unit, &reg, &sigs, &mut diags);
                    check_unused_vars(&f.body, unit, &mut diags);
                }
                Item::Test(t) => {
                    let mut env: HashMap<String, Type> = HashMap::new();
                    check_block_fields(&t.body, &mut env, unit, &reg, &sigs, &mut diags);
                    check_unused_vars(&t.body, unit, &mut diags);
                }
                Item::Goal(g) => {
                    check_goal(g, unit, &mut diags);
                    if g.plan.is_some() {
                        check_plan_goal(g, unit, &mut diags);
                    }
                }
                _ => {}
            }
        }
    }

    diags
}

/// The conditions a `require { ... }` block understands. An unknown condition can
/// never be satisfied, so naming one is almost certainly a mistake — we surface
/// it as a warning with the closest known set.
pub const KNOWN_REQUIRE_CONDS: &[&str] = &[
    "tests.pass",
    "no_new_effects",
    "no_forbidden_effects",
    "check.clean",
    // Coding-harness predicates, evaluated by the guard runtime.
    "no_out_of_scope_writes",
    "no_unapproved_effects",
];

/// Validate a `goal` declaration. Goal validation is deliberately permissive
/// about *effects* (an `allow` list may name effects the current builtin set
/// doesn't implement yet, e.g. `stripe.refund`, which is forward-looking by
/// design) but strict about *require* conditions, which must be ones the runtime
/// can actually evaluate.
fn check_goal(g: &GoalDecl, unit: &Unit, diags: &mut Vec<Diagnostic>) {
    // A plan goal's success conditions use a different, not-yet-enforced vocabulary
    // (e.g. `refunds.receipted`), so the repair-loop condition check doesn't apply.
    // Its plan is validated separately by `check_plan_goal`.
    if g.plan.is_none() {
        for c in &g.require.conditions {
            // The parameterized `command("…").passes` form is a coding-goal success
            // condition the guard runtime evaluates; only `passes` is supported.
            if c.arg.is_some() {
                if c.name != "command" || c.pred.as_deref() != Some("passes") {
                    diags.push(
                        Diagnostic::warning(
                            "E0431",
                            "unknown_require_condition",
                            format!(
                                "`{}(...)` is not a success condition the runtime can check",
                                c.name
                            ),
                            &unit.source.path,
                            c.span,
                        )
                        .with_note("the only parameterized condition is `command(\"…\").passes`"),
                    );
                }
                continue;
            }
            if !KNOWN_REQUIRE_CONDS.contains(&c.name.as_str()) {
                diags.push(
                    Diagnostic::warning(
                        "E0431",
                        "unknown_require_condition",
                        format!(
                            "`{}` is not a success condition the runtime can check",
                            c.name
                        ),
                        &unit.source.path,
                        c.span,
                    )
                    .with_note(format!(
                        "known conditions: {}",
                        KNOWN_REQUIRE_CONDS.join(", ")
                    )),
                );
            }
        }
    }
    // A goal with no budget at all will run unbounded; nudge the author.
    let b = &g.budget;
    if b.steps.is_none() && b.retries.is_none() {
        diags.push(
            Diagnostic::warning(
                "E0432",
                "goal_unbounded",
                format!("goal `{}` declares no step or retry budget", g.name),
                &unit.source.path,
                g.name_span,
            )
            .with_note("add a `budget { steps: N }` block so the run is bounded"),
        );
    }
}

/// Validate a single named goal — its require/budget and, if present, its plan
/// body. Returns `None` if the program has no goal by that name. Powers
/// `perdure goal check <name>`.
pub fn check_named_goal(program: &Program, name: &str) -> Option<Vec<Diagnostic>> {
    for unit in &program.units {
        for item in &unit.module.items {
            if let Item::Goal(g) = item {
                if g.name == name {
                    let mut diags = Vec::new();
                    check_goal(g, unit, &mut diags);
                    if g.plan.is_some() {
                        check_plan_goal(g, unit, &mut diags);
                    }
                    return Some(diags);
                }
            }
        }
    }
    None
}

/// Statically validate a goal's `plan { … }` block so a bad plan fails *before*
/// runtime. Deliberately tuned to never reject a plan the runtime would happily
/// run: its scope model matches the interpreter's single flat `Env` (let-names and
/// for-vars accumulate and never go out of scope), and its unsupported-expression
/// set mirrors `plan::eval_expr` exactly.
pub fn check_plan_goal(g: &GoalDecl, unit: &Unit, diags: &mut Vec<Diagnostic>) {
    let plan = match &g.plan {
        Some(p) => p,
        None => return,
    };
    let mut granted: BTreeSet<String> = g.allow.tools.iter().cloned().collect();
    // `shell.run` is granted by a non-empty `allow { shell.run [...] }` command
    // list, not by name in `tools` — mirror `tool::tool_granted`.
    if !g.allow.shell.is_empty() {
        granted.insert("shell.run".into());
    }
    let known: Vec<String> = crate::tool::known_tools()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut bound: BTreeSet<String> = BTreeSet::new();
    check_plan_stmts(
        &plan.stmts,
        &granted,
        &g.allow.shell,
        &known,
        &mut bound,
        unit,
        diags,
    );
}

fn check_plan_stmts(
    stmts: &[PlanStmt],
    granted: &BTreeSet<String>,
    shell: &[String],
    known: &[String],
    bound: &mut BTreeSet<String>,
    unit: &Unit,
    diags: &mut Vec<Diagnostic>,
) {
    for s in stmts {
        match s {
            PlanStmt::Let { name, value, .. } => {
                match value {
                    PlanValue::Call(c) => {
                        check_plan_call(c, granted, shell, known, bound, unit, diags)
                    }
                    PlanValue::Expr(e) => check_plan_expr(e, bound, unit, diags),
                }
                // The binding is visible only AFTER its right-hand side (a `let x = x`
                // reads the prior `x`), matching the interpreter.
                bound.insert(name.clone());
            }
            PlanStmt::Call { call, .. } => {
                check_plan_call(call, granted, shell, known, bound, unit, diags)
            }
            PlanStmt::Approve { body, .. } => {
                check_plan_stmts(body, granted, shell, known, bound, unit, diags)
            }
            PlanStmt::If {
                cond, then, els, ..
            } => {
                check_plan_expr(cond, bound, unit, diags);
                check_plan_stmts(then, granted, shell, known, bound, unit, diags);
                if let Some(els) = els {
                    check_plan_stmts(els, granted, shell, known, bound, unit, diags);
                }
            }
            PlanStmt::For {
                var, iter, body, ..
            } => {
                check_plan_expr(iter, bound, unit, diags);
                // The loop variable is in scope for the body (and, like the runtime's
                // flat Env, stays bound afterward — so the checker never over-rejects).
                bound.insert(var.clone());
                check_plan_stmts(body, granted, shell, known, bound, unit, diags);
            }
            PlanStmt::While { cond, body, span } => {
                check_plan_expr(cond, bound, unit, diags);
                if !plan_stmts_have_call(body) {
                    diags.push(
                        Diagnostic::warning(
                            "E0437",
                            "unbounded_plan_loop",
                            "this `while` loop makes no tool call, so it is bounded only by the run's iteration limit",
                            &unit.source.path,
                            *span,
                        )
                        .with_note("call a tool inside the loop so each iteration makes durable progress"),
                    );
                }
                check_plan_stmts(body, granted, shell, known, bound, unit, diags);
            }
        }
    }
}

fn check_plan_call(
    c: &PlanCall,
    granted: &BTreeSet<String>,
    shell: &[String],
    known: &[String],
    bound: &BTreeSet<String>,
    unit: &Unit,
    diags: &mut Vec<Diagnostic>,
) {
    if !known.contains(&c.tool) {
        let mut d = Diagnostic::error(
            "E0434",
            "unknown_tool",
            format!("`{}` is not a tool the runtime can invoke", c.tool),
            &unit.source.path,
            c.tool_span,
        );
        if let Some(near) = nearest_name(&c.tool, known) {
            d = d.with_note(format!("did you mean `{near}`?"));
        }
        diags.push(d);
    } else if !granted.contains(&c.tool) {
        let note = if c.tool == "shell.run" {
            "add `shell.run [\"<command>\"]` to the goal's `allow` block".to_string()
        } else {
            format!("add `{}` to the goal's `allow` block", c.tool)
        };
        diags.push(
            Diagnostic::error(
                "E0433",
                "tool_ungranted",
                format!(
                    "the plan calls `{}`, which the goal's `allow` block does not grant",
                    c.tool
                ),
                &unit.source.path,
                c.tool_span,
            )
            .with_note(note),
        );
    } else if c.tool == "shell.run" {
        // The command allowlist is exact-match; a literal `cmd` not on it will
        // be refused at runtime (authority.denied), so say it at check time.
        // Only a literal is checkable — a computed cmd is the runtime's job.
        if let Some((_, Expr::Str(cmd, span))) = c.input.iter().find(|(k, _)| k == "cmd") {
            if !shell.iter().any(|s| s == cmd) {
                diags.push(
                    Diagnostic::error(
                        "E0438",
                        "command_ungranted",
                        format!("`{cmd}` is not in the goal's `allow {{ shell.run [...] }}` list"),
                        &unit.source.path,
                        *span,
                    )
                    .with_note(format!(
                        "allowed commands: {}",
                        shell
                            .iter()
                            .map(|s| format!("`{s}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }
        }
    }
    for (_, e) in &c.input {
        check_plan_expr(e, bound, unit, diags);
    }
}

/// Walk a plan expression for unbound variables (E0435) and expression forms the
/// plan interpreter rejects (E0436 — exactly the set `plan::eval_expr` errors on).
fn check_plan_expr(e: &Expr, bound: &BTreeSet<String>, unit: &Unit, diags: &mut Vec<Diagnostic>) {
    match e {
        Expr::Int(..) | Expr::Float(..) | Expr::Str(..) | Expr::Bool(..) => {}
        Expr::Ident(name, span) => {
            if !bound.contains(name) {
                let cands: Vec<String> = bound.iter().cloned().collect();
                let mut d = Diagnostic::error(
                    "E0435",
                    "unbound_plan_var",
                    format!("`{name}` is not bound anywhere in this plan"),
                    &unit.source.path,
                    *span,
                );
                if let Some(near) = nearest_name(name, &cands) {
                    d = d.with_note(format!("did you mean `{near}`?"));
                }
                diags.push(d);
            }
        }
        Expr::Field { recv, .. } => check_plan_expr(recv, bound, unit, diags),
        Expr::Unary { expr, .. } => check_plan_expr(expr, bound, unit, diags),
        Expr::Binary { lhs, rhs, .. } => {
            check_plan_expr(lhs, bound, unit, diags);
            check_plan_expr(rhs, bound, unit, diags);
        }
        Expr::Record { fields, .. } => {
            for (_, fe) in fields {
                check_plan_expr(fe, bound, unit, diags);
            }
        }
        // The forms the plan interpreter does not evaluate. Message mirrors
        // `plan::eval_expr` so the static error reads like the runtime one.
        Expr::Call { span, .. } => diags.push(unsupported_plan_expr(
            "function calls are not allowed in a plan expression (use `call <tool> { ... }`)",
            unit,
            *span,
        )),
        Expr::Method {
            name, name_span, ..
        } => diags.push(unsupported_plan_expr(
            format!("method `.{name}()` is not supported in a plan expression"),
            unit,
            *name_span,
        )),
        Expr::Try { span, .. } => diags.push(unsupported_plan_expr(
            "`?` is not supported in a plan expression",
            unit,
            *span,
        )),
        Expr::Match { span, .. } => diags.push(unsupported_plan_expr(
            "`match` is not supported in a plan expression",
            unit,
            *span,
        )),
        Expr::Ok(_, span) | Expr::Err(_, span) => diags.push(unsupported_plan_expr(
            "`Ok`/`Err` are not supported in a plan expression",
            unit,
            *span,
        )),
    }
}

fn unsupported_plan_expr(msg: impl Into<String>, unit: &Unit, span: Span) -> Diagnostic {
    Diagnostic::error(
        "E0436",
        "unsupported_plan_expr",
        msg,
        &unit.source.path,
        span,
    )
}

/// Whether a plan body makes any tool call, recursing through nested control flow.
/// Used to flag a `while` loop that can only spin against the iteration limit.
fn plan_stmts_have_call(stmts: &[PlanStmt]) -> bool {
    stmts.iter().any(|s| match s {
        PlanStmt::Call { .. } => true,
        PlanStmt::Let {
            value: PlanValue::Call(_),
            ..
        } => true,
        PlanStmt::Let { .. } => false,
        PlanStmt::Approve { body, .. } => plan_stmts_have_call(body),
        PlanStmt::If { then, els, .. } => {
            plan_stmts_have_call(then) || els.as_deref().is_some_and(plan_stmts_have_call)
        }
        PlanStmt::For { body, .. } | PlanStmt::While { body, .. } => plan_stmts_have_call(body),
    })
}

fn build_sigs(program: &Program) -> HashMap<String, FnSig> {
    let mut m = HashMap::new();
    for u in &program.units {
        for it in &u.module.items {
            if let Item::Fn(f) = it {
                let effects = f
                    .effects
                    .as_ref()
                    .map(|c| c.effects.iter().map(|e| e.name.clone()).collect())
                    .unwrap_or_default();
                let ret = f.ret.as_ref().map(type_from_ast);
                m.insert(f.name.clone(), FnSig { effects, ret });
            }
        }
    }
    m
}

// ----- effect checking -----

fn check_fn_effects(
    f: &FnDecl,
    unit: &Unit,
    sigs: &HashMap<String, FnSig>,
    diags: &mut Vec<Diagnostic>,
) {
    let usage = analyze_block(&f.body, sigs);
    let declared: BTreeSet<String> = f
        .effects
        .as_ref()
        .map(|c| c.effects.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default();

    let missing: Vec<String> = usage.effects.difference(&declared).cloned().collect();
    if !missing.is_empty() {
        let union: BTreeSet<String> = declared.union(&usage.effects).cloned().collect();
        let union_list = union.iter().cloned().collect::<Vec<_>>().join(", ");

        let patch = match &f.effects {
            Some(clause) => PreferredPatch {
                file: unit.source.path.clone(),
                span: clause.list_span,
                replacement: union_list.clone(),
                rationale: format!(
                    "declare every effect this function performs: {}",
                    union_list
                ),
            },
            None => PreferredPatch {
                file: unit.source.path.clone(),
                span: Span::at(f.brace_offset),
                replacement: format!("effects [{}] ", union_list),
                rationale: format!("declare the effects this function performs: {}", union_list),
            },
        };

        let plural = if missing.len() > 1 { "s" } else { "" };
        let names = missing
            .iter()
            .map(|e| format!("`{}`", e))
            .collect::<Vec<_>>()
            .join(", ");
        let diag = Diagnostic::error(
            "E0421",
            "effect_undeclared",
            format!("function `{}` performs undeclared effect{} {}", f.name, plural, names),
            &unit.source.path,
            f.name_span,
        )
        .with_strategies(&["add_effect"])
        .with_patch(patch)
        .with_note("effects make a function's powers explicit to callers, reviewers, and agents — an agent can see at a glance that this function touches the DB or the network");
        diags.push(diag);
    }

    // unused declared effects are a lint, not an error
    let unused: Vec<String> = declared.difference(&usage.effects).cloned().collect();
    if !unused.is_empty() {
        if let Some(clause) = &f.effects {
            let (span, replacement) = if usage.effects.is_empty() {
                (clause.full_span, String::new())
            } else {
                (
                    clause.list_span,
                    usage.effects.iter().cloned().collect::<Vec<_>>().join(", "),
                )
            };
            let names = unused
                .iter()
                .map(|e| format!("`{}`", e))
                .collect::<Vec<_>>()
                .join(", ");
            let diag = Diagnostic::warning(
                "E0450",
                "effect_unused",
                format!(
                    "function `{}` declares unused effect{} {}",
                    f.name,
                    if unused.len() > 1 { "s" } else { "" },
                    names
                ),
                &unit.source.path,
                f.name_span,
            )
            .with_strategies(&["remove_effect"])
            .with_patch(PreferredPatch {
                file: unit.source.path.clone(),
                span,
                replacement,
                rationale: "remove effects the function does not actually perform".into(),
            });
            diags.push(diag);
        }
    }
}

fn unknown_module_diag(unit: &Unit, module: &str, span: Span) -> Diagnostic {
    let (patch_span, replacement) = if unit.module.last_import_end > 0 {
        (
            Span::at(unit.module.last_import_end),
            format!("\nimport {}", module),
        )
    } else {
        (Span::at(0), format!("import {}\n", module))
    };
    Diagnostic::error(
        "E0322",
        "unknown_module",
        format!("use of module `{}` which is not imported", module),
        &unit.source.path,
        span,
    )
    .with_strategies(&["add_import"])
    .with_patch(PreferredPatch {
        file: unit.source.path.clone(),
        span: patch_span,
        replacement,
        rationale: format!("import the `{}` module", module),
    })
    .with_note(format!("add `import {}` at the top of the file", module))
}

// ----- type checking -----

fn check_fn_types(
    f: &FnDecl,
    unit: &Unit,
    reg: &TypeRegistry,
    sigs: &HashMap<String, FnSig>,
    diags: &mut Vec<Diagnostic>,
) {
    let ret_ast = match &f.ret {
        Some(r) => r,
        None => return,
    };
    let declared = type_from_ast(ret_ast);
    let params: HashMap<String, Type> = f
        .params
        .iter()
        .map(|p| (p.name.clone(), type_from_ast(&p.ty)))
        .collect();

    let mut returns = Vec::new();
    collect_returns(&f.body, &mut returns);
    for rexpr in returns {
        let got = infer_expr(rexpr, &params, sigs, reg);
        if !compatible(&got, &declared, reg) {
            let patch = PreferredPatch {
                file: unit.source.path.clone(),
                span: ret_ast.span(),
                replacement: got.display(),
                rationale: format!(
                    "the returned value is `{}`, not `{}`",
                    got.display(),
                    declared.display()
                ),
            };
            let diag = Diagnostic::error(
                "E0309",
                "type_mismatch",
                format!(
                    "function `{}` returns `{}` but is declared to return `{}`",
                    f.name,
                    got.display(),
                    declared.display()
                ),
                &unit.source.path,
                rexpr.span(),
            )
            .with_strategies(&["fix_annotation", "convert_value"])
            .with_patch(patch)
            .with_note("either correct the return type annotation or convert the value");
            diags.push(diag);
            break; // one type error per function is enough to act on
        }
    }
}

fn collect_returns<'a>(block: &'a Block, out: &mut Vec<&'a Expr>) {
    for s in &block.stmts {
        match s {
            Stmt::Return { value: Some(e), .. } => out.push(e),
            Stmt::If { then, els, .. } => {
                collect_returns(then, out);
                if let Some(eb) = els {
                    collect_returns(eb, out);
                }
            }
            _ => {}
        }
    }
}

fn infer_expr(
    e: &Expr,
    params: &HashMap<String, Type>,
    sigs: &HashMap<String, FnSig>,
    reg: &TypeRegistry,
) -> Type {
    match e {
        Expr::Int(..) => Type::Int,
        Expr::Float(..) => Type::Float,
        Expr::Str(..) => Type::Str,
        Expr::Bool(..) => Type::Bool,
        Expr::Ident(name, _) => params
            .get(name)
            .cloned()
            .or_else(|| reg.enum_of_variant(name).map(|e| Type::Named(e.clone())))
            .unwrap_or(Type::Unknown),
        Expr::Unary { op, expr, .. } => match op {
            UnOp::Not => Type::Bool,
            UnOp::Neg => infer_expr(expr, params, sigs, reg),
        },
        Expr::Binary { op, lhs, .. } => match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => Type::Bool,
            _ => infer_expr(lhs, params, sigs, reg),
        },
        Expr::Field { recv, name, .. } => {
            let rt = infer_expr(recv, params, sigs, reg);
            field_type(&rt, name, reg)
        }
        Expr::Try { expr, .. } => match infer_expr(expr, params, sigs, reg) {
            Type::Result(ok, _) => *ok,
            _ => Type::Unknown,
        },
        Expr::Ok(inner, _) => Type::Result(
            Box::new(infer_expr(inner, params, sigs, reg)),
            Box::new(Type::Unknown),
        ),
        Expr::Err(inner, _) => Type::Result(
            Box::new(Type::Unknown),
            Box::new(infer_expr(inner, params, sigs, reg)),
        ),
        Expr::Record { name, fields, .. } => match name {
            Some(n) if reg.is_known(n) => Type::Named(n.clone()),
            _ => Type::Record(
                fields
                    .iter()
                    .map(|(fname, fe)| (fname.clone(), infer_expr(fe, params, sigs, reg)))
                    .collect(),
            ),
        },
        Expr::Call { callee, .. } => {
            if let Expr::Ident(fname, _) = &**callee {
                if fname == "to_string" {
                    return Type::Str;
                }
                if let Some(sig) = sigs.get(fname) {
                    return sig.ret.clone().unwrap_or(Type::Unknown);
                }
            }
            Type::Unknown
        }
        Expr::Method { recv, name, .. } => {
            if let Expr::Ident(m, _) = &**recv {
                if builtins::is_module(m) {
                    if let Some(b) = builtins::module_member(m, name) {
                        return b.ret;
                    }
                }
            }
            match name.as_str() {
                "is_ok" | "is_err" => Type::Bool,
                _ => Type::Unknown,
            }
        }
        // A `match` has the type of its arm bodies; infer from the first arm.
        Expr::Match { arms, .. } => arms
            .first()
            .map(|a| infer_expr(&a.body, params, sigs, reg))
            .unwrap_or(Type::Unknown),
    }
}

fn field_type(rt: &Type, name: &str, reg: &TypeRegistry) -> Type {
    match rt {
        Type::Named(n) => reg
            .record_fields(n)
            .and_then(|fields| fields.iter().find(|(fn_, _)| fn_ == name))
            .map(|(_, t)| t.clone())
            .unwrap_or(Type::Unknown),
        Type::Record(fields) => fields
            .iter()
            .find(|(fn_, _)| fn_ == name)
            .map(|(_, t)| t.clone())
            .unwrap_or(Type::Unknown),
        _ => Type::Unknown,
    }
}

// ----- unknown-field checking -----

/// Walk a block validating every field access against the receiver's type, while
/// threading a scope of locally-bound variable types so `let`-bound receivers
/// resolve. We only fire when the receiver resolves to a *known* record type;
/// `Unknown` (and opaque named types) are skipped, keeping the checker lenient.
fn check_block_fields(
    b: &Block,
    env: &mut HashMap<String, Type>,
    unit: &Unit,
    reg: &TypeRegistry,
    sigs: &HashMap<String, FnSig>,
    diags: &mut Vec<Diagnostic>,
) {
    for s in &b.stmts {
        match s {
            Stmt::Let { name, value, .. } => {
                check_expr_fields(value, env, unit, reg, sigs, diags);
                let t = infer_expr(value, env, sigs, reg);
                env.insert(name.clone(), t);
            }
            Stmt::Return { value: Some(e), .. } => {
                check_expr_fields(e, env, unit, reg, sigs, diags)
            }
            Stmt::Return { value: None, .. } => {}
            Stmt::Ensure { cond, els, .. } => {
                check_expr_fields(cond, env, unit, reg, sigs, diags);
                if let Some(e) = els {
                    check_expr_fields(e, env, unit, reg, sigs, diags);
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                check_expr_fields(cond, env, unit, reg, sigs, diags);
                let mut then_env = env.clone();
                check_block_fields(then, &mut then_env, unit, reg, sigs, diags);
                if let Some(eb) = els {
                    let mut else_env = env.clone();
                    check_block_fields(eb, &mut else_env, unit, reg, sigs, diags);
                }
            }
            Stmt::Expr(e) => check_expr_fields(e, env, unit, reg, sigs, diags),
        }
    }
}

fn check_expr_fields(
    e: &Expr,
    env: &HashMap<String, Type>,
    unit: &Unit,
    reg: &TypeRegistry,
    sigs: &HashMap<String, FnSig>,
    diags: &mut Vec<Diagnostic>,
) {
    match e {
        Expr::Field {
            recv,
            name,
            name_span,
            ..
        } => {
            check_expr_fields(recv, env, unit, reg, sigs, diags);
            let rt = infer_expr(recv, env, sigs, reg);
            if let Some(fields) = record_fields_of(&rt, reg) {
                if !fields.iter().any(|(fname, _)| fname == name) {
                    diags.push(unknown_field_diag(unit, &rt, name, *name_span, &fields));
                }
            }
        }
        Expr::Method { recv, args, .. } => {
            check_expr_fields(recv, env, unit, reg, sigs, diags);
            for a in args {
                check_expr_fields(a, env, unit, reg, sigs, diags);
            }
        }
        Expr::Call { callee, args, .. } => {
            check_expr_fields(callee, env, unit, reg, sigs, diags);
            for a in args {
                check_expr_fields(a, env, unit, reg, sigs, diags);
            }
        }
        Expr::Unary { expr, .. } | Expr::Try { expr, .. } => {
            check_expr_fields(expr, env, unit, reg, sigs, diags)
        }
        Expr::Binary { lhs, rhs, .. } => {
            check_expr_fields(lhs, env, unit, reg, sigs, diags);
            check_expr_fields(rhs, env, unit, reg, sigs, diags);
        }
        Expr::Ok(inner, _) | Expr::Err(inner, _) => {
            check_expr_fields(inner, env, unit, reg, sigs, diags)
        }
        Expr::Record { fields, .. } => {
            for (_, fe) in fields {
                check_expr_fields(fe, env, unit, reg, sigs, diags);
            }
        }
        Expr::Match {
            scrutinee,
            arms,
            span,
        } => {
            check_expr_fields(scrutinee, env, unit, reg, sigs, diags);
            for arm in arms {
                check_expr_fields(&arm.body, env, unit, reg, sigs, diags);
            }
            let st = infer_expr(scrutinee, env, sigs, reg);
            check_match(unit, &st, arms, *span, reg, diags);
        }
        Expr::Int(..) | Expr::Float(..) | Expr::Str(..) | Expr::Bool(..) | Expr::Ident(..) => {}
    }
}

/// Exhaustiveness + variant checks for a `match` whose scrutinee resolves to a
/// known enum. Unknown variants get a did-you-mean rename; a non-exhaustive
/// match gets a patch that inserts the missing arm(s). When the scrutinee type
/// is anything but a known enum we stay silent (no false positives).
fn check_match(
    unit: &Unit,
    scrutinee_ty: &Type,
    arms: &[MatchArm],
    match_span: Span,
    reg: &TypeRegistry,
    diags: &mut Vec<Diagnostic>,
) {
    let enum_name = match scrutinee_ty {
        Type::Named(n) if reg.is_enum(n) => n.clone(),
        _ => return,
    };
    let variants = reg.enum_variants(&enum_name).cloned().unwrap_or_default();
    let mut has_wildcard = false;
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for arm in arms {
        match &arm.pattern {
            Pattern::Wildcard { .. } => has_wildcard = true,
            Pattern::Variant { name, span } => {
                if variants.iter().any(|v| v == name) {
                    covered.insert(name.clone());
                } else {
                    diags.push(unknown_variant_diag(
                        unit, &enum_name, name, *span, &variants,
                    ));
                }
            }
        }
    }
    if !has_wildcard {
        let missing: Vec<String> = variants
            .iter()
            .filter(|v| !covered.contains(*v))
            .cloned()
            .collect();
        if !missing.is_empty() {
            diags.push(non_exhaustive_diag(
                unit, &enum_name, &missing, arms, match_span,
            ));
        }
    }
}

fn unknown_variant_diag(
    unit: &Unit,
    enum_name: &str,
    variant: &str,
    name_span: Span,
    variants: &[String],
) -> Diagnostic {
    let mut diag = Diagnostic::error(
        "E0341",
        "unknown_variant",
        format!("enum `{}` has no variant `{}`", enum_name, variant),
        &unit.source.path,
        name_span,
    )
    .with_note(format!("variants: {}", variants.join(", ")));
    if let Some(best) = nearest_name(variant, variants) {
        diag = diag
            .with_strategies(&["rename_variant"])
            .with_patch(PreferredPatch {
                file: unit.source.path.clone(),
                span: name_span,
                replacement: best.clone(),
                rationale: format!("did you mean `{}`?", best),
            });
    }
    diag
}

fn non_exhaustive_diag(
    unit: &Unit,
    enum_name: &str,
    missing: &[String],
    arms: &[MatchArm],
    match_span: Span,
) -> Diagnostic {
    let names = missing
        .iter()
        .map(|m| format!("`{}`", m))
        .collect::<Vec<_>>()
        .join(", ");
    // Underline just the `match` keyword rather than the whole expression.
    let head = Span::new(match_span.start, match_span.start + "match".len());
    let mut diag = Diagnostic::error(
        "E0340",
        "non_exhaustive_match",
        format!(
            "match on `{}` is not exhaustive; missing {}",
            enum_name, names
        ),
        &unit.source.path,
        head,
    )
    .with_strategies(&["add_arm"])
    .with_note("a match must cover every variant, or use a `_` catch-all arm");
    if let Some(patch) = insert_arm_patch(unit, arms, missing) {
        // Unlike the other mechanical fixes, this one's body is a *placeholder*: it
        // reuses the first arm's expression because that is the only generically
        // type-correct value available, so the patch makes the match compile and
        // exhaustive but does NOT synthesize the right behavior for the new variant.
        // Flag it unmistakably for whoever (human or agent) reads this diagnostic.
        // (The verify pipeline is a backstop: a placeholder that regresses a test is
        // rejected, so the loop never greens on a wrong body a test actually covers.)
        diag = diag
            .with_note(
                "the inserted arm body is a PLACEHOLDER (the first arm's value) — it \
                 compiles but is almost certainly wrong for the new variant; replace \
                 each placeholder body before relying on it",
            )
            .with_patch(patch);
    }
    diag
}

/// Build a patch that appends an arm for each missing variant after the last
/// existing arm, reusing the first arm's body as a type-correct placeholder the
/// author then fills in. Indentation is copied from the first arm's line.
fn insert_arm_patch(unit: &Unit, arms: &[MatchArm], missing: &[String]) -> Option<PreferredPatch> {
    let first = arms.first()?;
    let last = arms.last()?;
    let text = &unit.source.text;
    let indent = line_indent(text, first.span.start);
    let bspan = first.body.span();
    let body = text.get(bspan.start..bspan.end)?;
    let mut ins = String::new();
    for m in missing {
        ins.push('\n');
        ins.push_str(&indent);
        ins.push_str(m);
        ins.push_str(" => ");
        ins.push_str(body);
    }
    Some(PreferredPatch {
        file: unit.source.path.clone(),
        span: Span::at(last.span.end),
        replacement: ins,
        rationale: format!(
            "add the missing arm{}: {} (replace the placeholder body)",
            if missing.len() > 1 { "s" } else { "" },
            missing.join(", ")
        ),
    })
}

/// The leading whitespace of the line containing `offset`.
fn line_indent(text: &str, offset: usize) -> String {
    let off = offset.min(text.len());
    let start = text[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[start..off]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// Nearest candidate name to `name` by edit distance, if close enough to be a
/// plausible typo. Shared by unknown-field and unknown-variant suggestions.
fn nearest_name(name: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        let d = edit_distance(name, c);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c.as_str()));
        }
    }
    match best {
        Some((d, c)) if d > 0 && (d <= 2 || d * 2 <= name.chars().count()) => Some(c.to_string()),
        _ => None,
    }
}

/// The fields of `t` if it is a known record (named-and-declared, or structural),
/// else `None`. `Unknown` and opaque named types deliberately return `None`.
fn record_fields_of(t: &Type, reg: &TypeRegistry) -> Option<Vec<(String, Type)>> {
    match t {
        Type::Named(n) => reg.record_fields(n).cloned(),
        Type::Record(fields) => Some(fields.clone()),
        _ => None,
    }
}

fn unknown_field_diag(
    unit: &Unit,
    recv_ty: &Type,
    field: &str,
    name_span: Span,
    fields: &[(String, Type)],
) -> Diagnostic {
    let available = fields
        .iter()
        .map(|(n, _)| n.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let mut diag = Diagnostic::error(
        "E0330",
        "unknown_field",
        format!("type `{}` has no field `{}`", recv_ty.display(), field),
        &unit.source.path,
        name_span,
    )
    .with_note(format!("available fields: {}", available));

    // Only propose a rename when a field is a plausible typo away. Never guess
    // wildly — an agent would dutifully apply a bad rename.
    if let Some(best) = nearest_field(field, fields) {
        diag = diag
            .with_strategies(&["rename_field"])
            .with_patch(PreferredPatch {
                file: unit.source.path.clone(),
                span: name_span,
                replacement: best.clone(),
                rationale: format!("did you mean `{}`?", best),
            });
    }
    diag
}

/// The closest field name to `field` by edit distance, if one is near enough to
/// be a plausible typo. Ties resolve to the first field in declared order.
fn nearest_field(field: &str, fields: &[(String, Type)]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for (name, _) in fields {
        let d = edit_distance(field, name);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, name.as_str()));
        }
    }
    match best {
        Some((d, name)) if d > 0 && (d <= 2 || d * 2 <= field.chars().count()) => {
            Some(name.to_string())
        }
        _ => None,
    }
}

/// Levenshtein edit distance over Unicode scalar values.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ----- unused-import / unused-variable lints -----

fn unused_import_diag(unit: &Unit, im: &Import) -> Diagnostic {
    // Remove the whole line, trailing newline included, so no blank line is left.
    let text = &unit.source.text;
    let after = im.span.end.min(text.len());
    let line_end = text[after..]
        .find('\n')
        .map(|i| after + i + 1)
        .unwrap_or(text.len());
    Diagnostic::warning(
        "E0460",
        "unused_import",
        format!("module `{}` is imported but never used", im.module),
        &unit.source.path,
        im.span,
    )
    .with_strategies(&["remove_import"])
    .with_patch(PreferredPatch {
        file: unit.source.path.clone(),
        span: Span::new(im.span.start, line_end),
        replacement: String::new(),
        rationale: format!("remove the unused `import {}`", im.module),
    })
    .with_note("an unused import widens a file's apparent surface for no reason")
}

/// Flag `let` bindings never referenced in the body. The repair prefixes `_`
/// rather than deleting the binding — that silences the lint while preserving any
/// effect the right-hand side performs, so the patch can never change behavior.
fn check_unused_vars(b: &Block, unit: &Unit, diags: &mut Vec<Diagnostic>) {
    let refs = referenced_idents(b);
    collect_unused_lets(b, &refs, unit, diags);
}

fn collect_unused_lets(
    b: &Block,
    refs: &BTreeSet<String>,
    unit: &Unit,
    diags: &mut Vec<Diagnostic>,
) {
    for s in &b.stmts {
        match s {
            Stmt::Let {
                name, name_span, ..
            } => {
                if !name.starts_with('_') && !refs.contains(name) {
                    diags.push(unused_variable_diag(unit, name, *name_span));
                }
            }
            Stmt::If { then, els, .. } => {
                collect_unused_lets(then, refs, unit, diags);
                if let Some(eb) = els {
                    collect_unused_lets(eb, refs, unit, diags);
                }
            }
            _ => {}
        }
    }
}

fn unused_variable_diag(unit: &Unit, name: &str, name_span: Span) -> Diagnostic {
    Diagnostic::warning(
        "E0461",
        "unused_variable",
        format!("variable `{}` is never used", name),
        &unit.source.path,
        name_span,
    )
    .with_strategies(&["underscore_prefix"])
    .with_patch(PreferredPatch {
        file: unit.source.path.clone(),
        span: name_span,
        replacement: format!("_{}", name),
        rationale: format!("prefix with `_` to mark `{}` intentionally unused", name),
    })
}

/// Every identifier *referenced* anywhere in a block. A `let` binding's own name
/// is not a reference (it lives in `Stmt::Let.name`, not an `Expr::Ident`), so a
/// binding that never appears here is genuinely unused.
fn referenced_idents(b: &Block) -> BTreeSet<String> {
    fn we(e: &Expr, out: &mut BTreeSet<String>) {
        match e {
            Expr::Ident(n, _) => {
                out.insert(n.clone());
            }
            Expr::Unary { expr, .. } | Expr::Try { expr, .. } => we(expr, out),
            Expr::Binary { lhs, rhs, .. } => {
                we(lhs, out);
                we(rhs, out);
            }
            Expr::Call { callee, args, .. } => {
                we(callee, out);
                for a in args {
                    we(a, out);
                }
            }
            Expr::Method { recv, args, .. } => {
                we(recv, out);
                for a in args {
                    we(a, out);
                }
            }
            Expr::Field { recv, .. } => we(recv, out),
            Expr::Ok(e, _) | Expr::Err(e, _) => we(e, out),
            Expr::Record { fields, .. } => {
                for (_, fe) in fields {
                    we(fe, out);
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                we(scrutinee, out);
                for arm in arms {
                    we(&arm.body, out);
                }
            }
            Expr::Int(..) | Expr::Float(..) | Expr::Str(..) | Expr::Bool(..) => {}
        }
    }
    fn ws(s: &Stmt, out: &mut BTreeSet<String>) {
        match s {
            Stmt::Let { value, .. } => we(value, out),
            Stmt::Return { value: Some(e), .. } => we(e, out),
            Stmt::Return { value: None, .. } => {}
            Stmt::Ensure { cond, els, .. } => {
                we(cond, out);
                if let Some(e) = els {
                    we(e, out);
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                we(cond, out);
                for st in &then.stmts {
                    ws(st, out);
                }
                if let Some(eb) = els {
                    for st in &eb.stmts {
                        ws(st, out);
                    }
                }
            }
            Stmt::Expr(e) => we(e, out),
        }
    }
    let mut out = BTreeSet::new();
    for s in &b.stmts {
        ws(s, &mut out);
    }
    out
}

// ----- shared body walker -----

fn analyze_block(b: &Block, sigs: &HashMap<String, FnSig>) -> Usage {
    let mut u = Usage::default();
    walk_block(b, sigs, &mut u);
    u
}

fn walk_block(b: &Block, sigs: &HashMap<String, FnSig>, u: &mut Usage) {
    for s in &b.stmts {
        walk_stmt(s, sigs, u);
    }
}

fn walk_stmt(s: &Stmt, sigs: &HashMap<String, FnSig>, u: &mut Usage) {
    match s {
        Stmt::Let { value, .. } => walk_expr(value, sigs, u),
        Stmt::Return { value: Some(e), .. } => walk_expr(e, sigs, u),
        Stmt::Return { value: None, .. } => {}
        Stmt::Ensure { cond, els, .. } => {
            walk_expr(cond, sigs, u);
            if let Some(e) = els {
                walk_expr(e, sigs, u);
            }
        }
        Stmt::If {
            cond, then, els, ..
        } => {
            walk_expr(cond, sigs, u);
            walk_block(then, sigs, u);
            if let Some(eb) = els {
                walk_block(eb, sigs, u);
            }
        }
        Stmt::Expr(e) => walk_expr(e, sigs, u),
    }
}

fn walk_expr(e: &Expr, sigs: &HashMap<String, FnSig>, u: &mut Usage) {
    match e {
        Expr::Method {
            recv,
            name,
            args,
            span,
            ..
        } => {
            if let Expr::Ident(m, _) = &**recv {
                if builtins::is_module(m) {
                    u.module_uses.entry(m.clone()).or_insert(*span);
                    if let Some(b) = builtins::module_member(m, name) {
                        if let Some(eff) = b.effect {
                            u.effects.insert(eff.to_string());
                        }
                    }
                }
            }
            walk_expr(recv, sigs, u);
            for a in args {
                walk_expr(a, sigs, u);
            }
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(fname, _) = &**callee {
                if let Some(sig) = sigs.get(fname) {
                    for eff in &sig.effects {
                        u.effects.insert(eff.clone());
                    }
                }
            }
            walk_expr(callee, sigs, u);
            for a in args {
                walk_expr(a, sigs, u);
            }
        }
        Expr::Field { recv, .. } => walk_expr(recv, sigs, u),
        Expr::Try { expr, .. } => walk_expr(expr, sigs, u),
        Expr::Unary { expr, .. } => walk_expr(expr, sigs, u),
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, sigs, u);
            walk_expr(rhs, sigs, u);
        }
        Expr::Ok(e, _) | Expr::Err(e, _) => walk_expr(e, sigs, u),
        Expr::Record { fields, .. } => {
            for (_, fe) in fields {
                walk_expr(fe, sigs, u);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            walk_expr(scrutinee, sigs, u);
            for arm in arms {
                walk_expr(&arm.body, sigs, u);
            }
        }
        Expr::Int(..) | Expr::Float(..) | Expr::Str(..) | Expr::Bool(..) | Expr::Ident(..) => {}
    }
}

// ----- analysis helpers reused by the patch pipeline & agent loop -----

/// The set of effects actually performed anywhere in the program (inferred from
/// bodies, independent of what is declared). Used to detect when a patch would
/// introduce a brand-new effect into the codebase.
pub fn used_effects(program: &Program) -> BTreeSet<String> {
    let sigs = build_sigs(program);
    let mut all = BTreeSet::new();
    for u in &program.units {
        for it in &u.module.items {
            if let Item::Fn(f) = it {
                all.extend(analyze_block(&f.body, &sigs).effects);
            }
        }
    }
    all
}

/// Names invoked via `name(...)` anywhere in a block (callees only — not methods).
pub fn called_names_in_block(b: &Block) -> BTreeSet<String> {
    fn we(e: &Expr, out: &mut BTreeSet<String>) {
        match e {
            Expr::Call { callee, args, .. } => {
                if let Expr::Ident(n, _) = &**callee {
                    out.insert(n.clone());
                }
                we(callee, out);
                for a in args {
                    we(a, out);
                }
            }
            Expr::Method { recv, args, .. } => {
                we(recv, out);
                for a in args {
                    we(a, out);
                }
            }
            Expr::Field { recv, .. } => we(recv, out),
            Expr::Try { expr, .. } | Expr::Unary { expr, .. } => we(expr, out),
            Expr::Binary { lhs, rhs, .. } => {
                we(lhs, out);
                we(rhs, out);
            }
            Expr::Ok(e, _) | Expr::Err(e, _) => we(e, out),
            Expr::Record { fields, .. } => {
                for (_, fe) in fields {
                    we(fe, out);
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                we(scrutinee, out);
                for arm in arms {
                    we(&arm.body, out);
                }
            }
            _ => {}
        }
    }
    fn ws(s: &Stmt, out: &mut BTreeSet<String>) {
        match s {
            Stmt::Let { value, .. } => we(value, out),
            Stmt::Return { value: Some(e), .. } => we(e, out),
            Stmt::Return { value: None, .. } => {}
            Stmt::Ensure { cond, els, .. } => {
                we(cond, out);
                if let Some(e) = els {
                    we(e, out);
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                we(cond, out);
                for st in &then.stmts {
                    ws(st, out);
                }
                if let Some(eb) = els {
                    for st in &eb.stmts {
                        ws(st, out);
                    }
                }
            }
            Stmt::Expr(e) => we(e, out),
        }
    }
    let mut out = BTreeSet::new();
    for s in &b.stmts {
        ws(s, &mut out);
    }
    out
}

/// A stable textual signature for public-API-change detection.
pub fn signature_string(f: &FnDecl) -> String {
    let params = f
        .params
        .iter()
        .map(|p| type_from_ast(&p.ty).display())
        .collect::<Vec<_>>()
        .join(", ");
    let ret = f
        .ret
        .as_ref()
        .map(|r| type_from_ast(r).display())
        .unwrap_or_else(|| "Unit".into());
    let effects = f
        .effects
        .as_ref()
        .map(|c| {
            let mut e: Vec<String> = c.effects.iter().map(|x| x.name.clone()).collect();
            e.sort();
            format!(" effects [{}]", e.join(", "))
        })
        .unwrap_or_default();
    format!("({}) -> {}{}", params, ret, effects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceFile;

    const BROKEN: &str = r#"
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

    #[test]
    fn finds_the_three_planted_bugs() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("auth.pdr", BROKEN)]);
        let diags = check_program(&prog);
        let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        let kinds: BTreeSet<&str> = errors.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains("unknown_module"), "diags: {:?}", errors);
        assert!(kinds.contains("effect_undeclared"), "diags: {:?}", errors);
        assert!(kinds.contains("type_mismatch"), "diags: {:?}", errors);
        assert_eq!(errors.len(), 3, "expected exactly 3 errors: {:?}", errors);

        // every error must carry a machine-applicable patch
        for d in &errors {
            assert!(d.preferred_patch.is_some(), "no patch on {:?}", d);
        }

        // the effect patch should declare all three effects in sorted order
        let eff = errors
            .iter()
            .find(|d| d.kind == "effect_undeclared")
            .unwrap();
        let patch = eff.preferred_patch.as_ref().unwrap();
        assert_eq!(
            patch.replacement,
            "effects [db.read, log.write, time.read] "
        );

        // the type patch should correct String -> Int
        let ty = errors.iter().find(|d| d.kind == "type_mismatch").unwrap();
        assert_eq!(ty.preferred_patch.as_ref().unwrap().replacement, "Int");
    }

    const FIELD_TYPO: &str = r#"
type Session = {
  token: String
  user_id: Int
  expires_at: Int
}

fn summary(s: Session) -> Int {
  return s.user_idx
}
"#;

    #[test]
    fn unknown_field_suggests_nearest() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("f.pdr", FIELD_TYPO)]);
        let diags = check_program(&prog);
        let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        let uf = errors
            .iter()
            .find(|d| d.kind == "unknown_field")
            .expect("an unknown_field error");
        assert_eq!(uf.code, "E0330");
        // the patch renames the typo to the nearest real field …
        let patch = uf.preferred_patch.as_ref().expect("a rename patch");
        assert_eq!(patch.replacement, "user_id");
        // … and targets exactly the field-name identifier, nothing more.
        assert_eq!(&FIELD_TYPO[patch.span.start..patch.span.end], "user_idx");
    }

    const FIELD_FAR: &str = r#"
type Session = {
  token: String
  user_id: Int
}

fn summary(s: Session) -> Int {
  return s.zzzzzzz
}
"#;

    #[test]
    fn unknown_field_does_not_guess_wildly() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("f.pdr", FIELD_FAR)]);
        let diags = check_program(&prog);
        let uf = diags
            .iter()
            .find(|d| d.kind == "unknown_field")
            .expect("an unknown_field error");
        assert!(
            uf.preferred_patch.is_none(),
            "should not propose a far-fetched rename"
        );
    }

    #[test]
    fn known_record_with_unknown_receiver_is_lenient() {
        // `row` comes from db.query, whose row type is Unknown — accessing fields
        // on it must NOT produce a false positive (the demo relies on this).
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("auth.pdr", BROKEN)]);
        let diags = check_program(&prog);
        assert!(
            !diags.iter().any(|d| d.kind == "unknown_field"),
            "no unknown_field on Unknown-typed receivers: {:?}",
            diags
        );
    }

    const UNUSED_IMPORT: &str = "\nimport math\n\nfn double(x: Int) -> Int {\n  return x\n}\n";

    #[test]
    fn unused_import_is_removed() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", UNUSED_IMPORT)]);
        let diags = check_program(&prog);
        let d = diags
            .iter()
            .find(|d| d.kind == "unused_import")
            .expect("an unused_import warning");
        assert_eq!(d.code, "E0460");
        assert!(!d.is_error(), "unused_import is a lint, not an error");
        let patch = d.preferred_patch.as_ref().expect("a remove patch");
        assert_eq!(patch.replacement, "");
        // the patch deletes exactly the import line, trailing newline included.
        assert_eq!(
            &UNUSED_IMPORT[patch.span.start..patch.span.end],
            "import math\n"
        );
    }

    const UNUSED_VAR: &str = r#"
fn compute(x: Int) -> Int {
  let scratch = x
  return x
}
"#;

    #[test]
    fn unused_variable_gets_underscore() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", UNUSED_VAR)]);
        let diags = check_program(&prog);
        let d = diags
            .iter()
            .find(|d| d.kind == "unused_variable")
            .expect("an unused_variable warning");
        assert_eq!(d.code, "E0461");
        assert!(!d.is_error());
        let patch = d.preferred_patch.as_ref().expect("an underscore patch");
        assert_eq!(patch.replacement, "_scratch");
        assert_eq!(&UNUSED_VAR[patch.span.start..patch.span.end], "scratch");
    }

    const NON_EXHAUSTIVE: &str = "type Signal = Stop | Go\n\nfn allow(s: Signal) -> Bool {\n  return match s {\n    Stop => false\n  }\n}\n";

    #[test]
    fn non_exhaustive_match_inserts_missing_arm() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", NON_EXHAUSTIVE)]);
        let diags = check_program(&prog);
        let d = diags
            .iter()
            .find(|d| d.kind == "non_exhaustive_match")
            .expect("a non_exhaustive_match error");
        assert_eq!(d.code, "E0340");
        let patch = d.preferred_patch.as_ref().expect("an add-arm patch");
        // inserts `Go` with the first arm's body as a placeholder, 4-space indent
        assert_eq!(patch.replacement, "\n    Go => false");
        // the insertion point is the end of the last existing arm (`Stop => false`)
        let at = NON_EXHAUSTIVE.find("=> false").unwrap() + "=> false".len();
        assert_eq!(patch.span.start, at);
        assert_eq!(patch.span.end, at);
    }

    const TYPO_VARIANT: &str = "type Signal = Stop | Go\n\nfn allow(s: Signal) -> Bool {\n  return match s {\n    Stop => false\n    Goo => true\n    _ => false\n  }\n}\n";

    #[test]
    fn unknown_variant_suggests_nearest() {
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", TYPO_VARIANT)]);
        let diags = check_program(&prog);
        let d = diags
            .iter()
            .find(|d| d.kind == "unknown_variant")
            .expect("an unknown_variant error");
        assert_eq!(d.code, "E0341");
        let patch = d.preferred_patch.as_ref().expect("a rename patch");
        assert_eq!(patch.replacement, "Go");
        assert_eq!(&TYPO_VARIANT[patch.span.start..patch.span.end], "Goo");
        // a wildcard arm is present, so the match is NOT also non-exhaustive.
        assert!(!diags.iter().any(|d| d.kind == "non_exhaustive_match"));
    }

    #[test]
    fn exhaustive_match_is_clean() {
        const OK: &str = "type Signal = Stop | Go\n\nfn allow(s: Signal) -> Bool {\n  return match s {\n    Stop => false\n    Go => true\n  }\n}\n";
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", OK)]);
        let diags = check_program(&prog);
        assert!(
            !diags
                .iter()
                .any(|d| d.kind == "non_exhaustive_match" || d.kind == "unknown_variant"),
            "fully-covered match should be clean: {:?}",
            diags
        );
    }

    #[test]
    fn used_variable_and_underscore_are_not_flagged() {
        // a referenced binding, and an already-underscored one, are both fine.
        const OK: &str = r#"
fn compute(x: Int) -> Int {
  let y = x
  let _ignored = x
  return y
}
"#;
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", OK)]);
        let diags = check_program(&prog);
        assert!(
            !diags.iter().any(|d| d.kind == "unused_variable"),
            "no unused_variable expected: {:?}",
            diags
        );
    }

    // ----- plan-goal checker (`perdure goal check`) -----

    fn plan_diag_kinds(src: &str) -> Vec<String> {
        let (prog, pd) = Program::parse_sources(vec![SourceFile::new("g.pdr", src)]);
        assert!(pd.iter().all(|d| !d.is_error()), "parse errors: {pd:?}");
        check_program(&prog)
            .iter()
            .map(|d| d.kind.clone())
            .collect()
    }

    /// A `while` retry loop that rebinds its condition variables (`attempt`,
    /// `result`) inside the body — the case most likely to provoke a false unbound
    /// error. It must check perfectly clean.
    const DEPLOY_SRC: &str = r#"goal LocalDeploy -> Success {
  budget { steps: 20 }
  allow { fake.ci.deploy fake.zendesk.comment }
  require { deploy.receipted }
  plan {
    let attempt = 1
    let result = call fake.ci.deploy { service: "api", attempt: attempt }
    while !result.ok && attempt < 5 {
      let attempt = attempt + 1
      let result = call fake.ci.deploy { service: "api", attempt: attempt }
    }
    approve "announce" {
      call fake.zendesk.comment { ticket_id: "zd", body: "done", public: true }
    }
  }
}"#;

    #[test]
    fn valid_plan_goals_check_clean() {
        // The scaffold's chargebacks demo and a rebinding while-loop both pass with
        // zero diagnostics — no false positives, and no E0431 on `refunds.receipted`.
        assert!(
            plan_diag_kinds(crate::project::PLAN_DEMO_CHARGEBACKS).is_empty(),
            "chargebacks demo should check clean"
        );
        assert!(
            plan_diag_kinds(DEPLOY_SRC).is_empty(),
            "rebinding while loop should check clean"
        );
    }

    #[test]
    fn plan_flags_ungranted_tool() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { fake.email.send }
  plan { call fake.stripe.refund { charge_id: "c", amount_cents: 1 } }
}"#;
        assert!(plan_diag_kinds(src).contains(&"tool_ungranted".to_string()));
    }

    #[test]
    fn plan_flags_unknown_tool_with_suggestion() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { fake.stripe.refundd }
  plan { call fake.stripe.refundd { charge_id: "c" } }
}"#;
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("g.pdr", src)]);
        let diags = check_program(&prog);
        let unknown = diags
            .iter()
            .find(|d| d.kind == "unknown_tool")
            .expect("E0434");
        assert!(
            unknown
                .notes
                .iter()
                .any(|n| n.contains("fake.stripe.refund")),
            "did-you-mean note: {:?}",
            unknown.notes
        );
    }

    #[test]
    fn plan_flags_unbound_variable() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { fake.email.send }
  plan { call fake.email.send { to: missing } }
}"#;
        assert!(plan_diag_kinds(src).contains(&"unbound_plan_var".to_string()));
    }

    #[test]
    fn plan_flags_unsupported_expression() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { fake.email.send }
  plan {
    let x = call fake.email.send { to: "a" }
    call fake.email.send { to: x.field.unwrap() }
  }
}"#;
        assert!(plan_diag_kinds(src).contains(&"unsupported_plan_expr".to_string()));
    }

    #[test]
    fn plan_flags_a_while_loop_with_no_call() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { fake.email.send }
  plan {
    let n = 0
    while n < 3 { let n = n + 1 }
  }
}"#;
        assert!(plan_diag_kinds(src).contains(&"unbounded_plan_loop".to_string()));
    }

    #[test]
    fn plan_goal_require_condition_is_not_flagged() {
        // `refunds.receipted` is a plan-goal condition, not a repair one — E0431
        // must not fire for a goal that carries a plan block.
        assert!(!plan_diag_kinds(crate::project::PLAN_DEMO_CHARGEBACKS)
            .contains(&"unknown_require_condition".to_string()));
    }

    #[test]
    fn shell_run_is_granted_by_a_nonempty_command_list() {
        // The shell demo template is the canonical exercise: shell.run is not in
        // `tools`, yet the non-empty command list grants it.
        let kinds = plan_diag_kinds(crate::project::PLAN_DEMO_SHELL);
        assert!(
            !kinds.contains(&"tool_ungranted".to_string())
                && !kinds.contains(&"unknown_tool".to_string())
                && !kinds.contains(&"command_ungranted".to_string()),
            "the shell template must check clean: {kinds:?}"
        );
    }

    #[test]
    fn shell_run_without_a_command_list_is_ungranted() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { fake.email.send }
  plan {
    call shell.run { cmd: "cargo test" }
  }
}"#;
        assert!(plan_diag_kinds(src).contains(&"tool_ungranted".to_string()));
    }

    #[test]
    fn literal_command_off_the_allowlist_is_flagged_at_check_time() {
        let src = r#"goal G -> Success {
  budget { steps: 5 }
  allow { shell.run ["cargo test"] }
  plan {
    call shell.run { cmd: "rm -rf /" }
  }
}"#;
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("m.pdr", src)]);
        let diags = check_program(&prog);
        let d = diags
            .iter()
            .find(|d| d.kind == "command_ungranted")
            .expect("an E0438 command_ungranted error");
        assert_eq!(d.code, "E0438");
        assert!(d.is_error());
    }
}
