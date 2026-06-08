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

        // --- per-function effect + type checks
        for item in &unit.module.items {
            if let Item::Fn(f) = item {
                check_fn_effects(f, unit, &sigs, &mut diags);
                check_fn_types(f, unit, &reg, &sigs, &mut diags);
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
}
