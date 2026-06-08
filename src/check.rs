//! The Tach checker: type and effect analysis.
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
                }
                Item::Test(t) => {
                    let mut env: HashMap<String, Type> = HashMap::new();
                    check_block_fields(&t.body, &mut env, unit, &reg, &sigs, &mut diags);
                }
                _ => {}
            }
        }
    }

    diags
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
        Expr::Ident(name, _) => params.get(name).cloned().unwrap_or(Type::Unknown),
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
        Expr::Int(..) | Expr::Float(..) | Expr::Str(..) | Expr::Bool(..) | Expr::Ident(..) => {}
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
        if best.map_or(true, |(bd, _)| d < bd) {
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
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("auth.tach", BROKEN)]);
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
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("f.tach", FIELD_TYPO)]);
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
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("f.tach", FIELD_FAR)]);
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
        let (prog, _) = Program::parse_sources(vec![SourceFile::new("auth.tach", BROKEN)]);
        let diags = check_program(&prog);
        assert!(
            !diags.iter().any(|d| d.kind == "unknown_field"),
            "no unknown_field on Unknown-typed receivers: {:?}",
            diags
        );
    }
}
