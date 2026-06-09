use crate::ast::*;
use crate::builtins;
use crate::program::Program;
use crate::span::Span;
use crate::value::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

/// Non-local control flow during evaluation. Modeled as the `Err` arm of a
/// `Result` so `?` in the interpreter itself does the propagation for us.
pub enum Signal {
    /// `return e` — unwound to the enclosing function call boundary.
    Return(Value),
    /// An `Err` value flowing out via `?` — caught at the function boundary and
    /// becomes that function's result.
    Propagate(Value),
    /// A bare `ensure` that failed (no `else`). Becomes an `Err` in a
    /// Result-returning function, or a test failure otherwise.
    Ensure(EnsureInfo),
    /// A genuine runtime error (bad field, division by zero, ...).
    Error(String, Span),
}

pub struct EnsureInfo {
    pub text: String,
    pub span: Span,
}

/// A simple lexically-scoped environment.
pub struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}

impl Env {
    pub fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: &str, v: Value) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), v);
    }

    fn get(&self, name: &str) -> Option<&Value> {
        for s in self.scopes.iter().rev() {
            if let Some(v) = s.get(name) {
                return Some(v);
            }
        }
        None
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

/// The tree-walking interpreter.
///
/// Fully deterministic: a fixed clock, no randomness, no real I/O. That is what
/// makes `tach replay` reproduce a run exactly and what makes the agent loop's
/// metrics trustworthy.
pub struct Interp<'a> {
    funcs: HashMap<String, &'a FnDecl>,
    /// Every sum-type variant name in the program — an identifier that names one
    /// (and isn't shadowed by a local) evaluates to a `Value::Variant`.
    variants: std::collections::HashSet<String>,
    db: RefCell<BTreeMap<String, Value>>,
    log: RefCell<Vec<String>>,
    now: i64,
}

impl<'a> Interp<'a> {
    pub fn new(program: &'a Program) -> Self {
        Interp {
            funcs: program.functions(),
            variants: program.type_registry().variants(),
            db: RefCell::new(BTreeMap::new()),
            log: RefCell::new(Vec::new()),
            now: 1000,
        }
    }

    /// Reset mutable world state between tests so they cannot leak into one another.
    pub fn reset_state(&self) {
        self.db.borrow_mut().clear();
        self.log.borrow_mut().clear();
    }

    pub fn logs(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    pub fn run_main(&self) -> Result<Value, Signal> {
        match self.funcs.get("main").copied() {
            Some(f) => self.call_fn(f, vec![]),
            None => Err(Signal::Error(
                "no `main` function to run".into(),
                Span::dummy(),
            )),
        }
    }

    fn fn_returns_result(f: &FnDecl) -> bool {
        matches!(&f.ret, Some(TypeExpr::Name { name, .. }) if name == "Result")
    }

    pub fn call_fn(&self, f: &FnDecl, args: Vec<Value>) -> Result<Value, Signal> {
        let mut env = Env::new();
        for (p, v) in f.params.iter().zip(args) {
            env.define(&p.name, v);
        }
        match self.eval_block(&f.body, &mut env) {
            Ok(_) => Ok(Value::Unit),
            Err(Signal::Return(v)) => Ok(v),
            Err(Signal::Propagate(errv)) => Ok(errv),
            Err(Signal::Ensure(info)) => {
                if Self::fn_returns_result(f) {
                    Ok(Value::Err(Box::new(Value::Str(format!(
                        "ensure failed: {}",
                        info.text
                    )))))
                } else {
                    Err(Signal::Ensure(info))
                }
            }
            Err(e) => Err(e),
        }
    }

    pub fn eval_block(&self, b: &Block, env: &mut Env) -> Result<Value, Signal> {
        env.push();
        let mut last = Value::Unit;
        let mut err = None;
        for s in &b.stmts {
            match self.eval_stmt(s, env) {
                Ok(v) => last = v,
                Err(sig) => {
                    err = Some(sig);
                    break;
                }
            }
        }
        env.pop();
        match err {
            Some(e) => Err(e),
            None => Ok(last),
        }
    }

    fn eval_stmt(&self, s: &Stmt, env: &mut Env) -> Result<Value, Signal> {
        match s {
            Stmt::Let { name, value, .. } => {
                let v = self.eval_expr(value, env)?;
                env.define(name, v);
                Ok(Value::Unit)
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.eval_expr(e, env)?,
                    None => Value::Unit,
                };
                Err(Signal::Return(v))
            }
            Stmt::Ensure { cond, els, .. } => {
                let c = self.eval_expr(cond, env)?;
                let b = c.as_bool().ok_or_else(|| {
                    Signal::Error(
                        format!("`ensure` expects a Bool, got {}", c.type_name()),
                        cond.span(),
                    )
                })?;
                if b {
                    Ok(Value::Unit)
                } else if let Some(e) = els {
                    let ev = self.eval_expr(e, env)?;
                    Err(Signal::Return(Value::Err(Box::new(ev))))
                } else {
                    Err(Signal::Ensure(EnsureInfo {
                        text: render_expr(cond),
                        span: cond.span(),
                    }))
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                let c = self.eval_expr(cond, env)?;
                let b = c.as_bool().ok_or_else(|| {
                    Signal::Error(
                        format!("`if` expects a Bool, got {}", c.type_name()),
                        cond.span(),
                    )
                })?;
                if b {
                    self.eval_block(then, env)
                } else if let Some(eb) = els {
                    self.eval_block(eb, env)
                } else {
                    Ok(Value::Unit)
                }
            }
            Stmt::Expr(e) => self.eval_expr(e, env),
        }
    }

    fn eval_expr(&self, e: &Expr, env: &mut Env) -> Result<Value, Signal> {
        match e {
            Expr::Int(n, _) => Ok(Value::Int(*n)),
            Expr::Float(x, _) => Ok(Value::Float(*x)),
            Expr::Str(s, _) => Ok(Value::Str(s.clone())),
            Expr::Bool(b, _) => Ok(Value::Bool(*b)),
            Expr::Ident(name, span) => env
                .get(name)
                .cloned()
                .or_else(|| {
                    // A bare identifier that names a sum-type variant (and isn't
                    // shadowed by a local) is that variant's value.
                    self.variants
                        .contains(name)
                        .then(|| Value::Variant(name.clone()))
                })
                .ok_or_else(|| Signal::Error(format!("unknown variable `{}`", name), *span)),
            Expr::Unary { op, expr, span } => {
                let v = self.eval_expr(expr, env)?;
                match op {
                    UnOp::Not => match v {
                        Value::Bool(b) => Ok(Value::Bool(!b)),
                        _ => Err(Signal::Error("`!` expects a Bool".into(), *span)),
                    },
                    UnOp::Neg => match v {
                        Value::Int(n) => Ok(Value::Int(-n)),
                        Value::Float(x) => Ok(Value::Float(-x)),
                        _ => Err(Signal::Error("`-` expects a number".into(), *span)),
                    },
                }
            }
            Expr::Binary { op, lhs, rhs, span } => self.eval_binary(*op, lhs, rhs, *span, env),
            Expr::Try { expr, span } => {
                let v = self.eval_expr(expr, env)?;
                match v {
                    Value::Ok(inner) => Ok(*inner),
                    Value::Err(e) => Err(Signal::Propagate(Value::Err(e))),
                    other => Err(Signal::Error(
                        format!("`?` expects a Result, got {}", other.type_name()),
                        *span,
                    )),
                }
            }
            Expr::Ok(inner, _) => {
                let v = self.eval_expr(inner, env)?;
                Ok(Value::Ok(Box::new(v)))
            }
            Expr::Err(inner, _) => {
                let v = self.eval_expr(inner, env)?;
                Ok(Value::Err(Box::new(v)))
            }
            Expr::Field {
                recv, name, span, ..
            } => {
                let r = self.eval_expr(recv, env)?;
                match r {
                    Value::Record(m) => m.get(name).cloned().ok_or_else(|| {
                        Signal::Error(format!("no field `{}` on record", name), *span)
                    }),
                    other => Err(Signal::Error(
                        format!("cannot access field `{}` on {}", name, other.type_name()),
                        *span,
                    )),
                }
            }
            Expr::Record { fields, .. } => {
                let mut m = BTreeMap::new();
                for (n, fe) in fields {
                    let v = self.eval_expr(fe, env)?;
                    m.insert(n.clone(), v);
                }
                Ok(Value::Record(m))
            }
            Expr::Call { callee, args, span } => self.eval_call(callee, args, *span, env),
            Expr::Method {
                recv,
                name,
                args,
                span,
                ..
            } => self.eval_method(recv, name, args, *span, env),
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                let v = self.eval_expr(scrutinee, env)?;
                for arm in arms {
                    if pattern_matches(&arm.pattern, &v) {
                        return self.eval_expr(&arm.body, env);
                    }
                }
                Err(Signal::Error(format!("no match arm for `{}`", v), *span))
            }
        }
    }

    fn eval_binary(
        &self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        env: &mut Env,
    ) -> Result<Value, Signal> {
        use BinOp::*;
        if op == And || op == Or {
            let l = self.eval_expr(lhs, env)?;
            let lb = l
                .as_bool()
                .ok_or_else(|| Signal::Error("logical operator expects Bool".into(), span))?;
            if op == And && !lb {
                return Ok(Value::Bool(false));
            }
            if op == Or && lb {
                return Ok(Value::Bool(true));
            }
            let r = self.eval_expr(rhs, env)?;
            let rb = r
                .as_bool()
                .ok_or_else(|| Signal::Error("logical operator expects Bool".into(), span))?;
            return Ok(Value::Bool(rb));
        }
        let l = self.eval_expr(lhs, env)?;
        let r = self.eval_expr(rhs, env)?;
        match op {
            Add | Sub | Mul | Div => self.arith(op, l, r, span),
            Eq => Ok(Value::Bool(value_eq(&l, &r))),
            Ne => Ok(Value::Bool(!value_eq(&l, &r))),
            Lt | Le | Gt | Ge => self.compare(op, l, r, span),
            And | Or => unreachable!(),
        }
    }

    fn arith(&self, op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, Signal> {
        use BinOp::*;
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    Add => a.checked_add(b),
                    Sub => a.checked_sub(b),
                    Mul => a.checked_mul(b),
                    Div => {
                        if b == 0 {
                            return Err(Signal::Error("division by zero".into(), span));
                        }
                        a.checked_div(b)
                    }
                    _ => None,
                };
                Ok(Value::Int(v.ok_or_else(|| {
                    Signal::Error("integer overflow".into(), span)
                })?))
            }
            (Value::Float(a), Value::Float(b)) => {
                let v = match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    Div => a / b,
                    _ => 0.0,
                };
                Ok(Value::Float(v))
            }
            (Value::Str(a), Value::Str(b)) if op == Add => Ok(Value::Str(a + &b)),
            (l, r) => Err(Signal::Error(
                format!(
                    "cannot apply arithmetic to {} and {}",
                    l.type_name(),
                    r.type_name()
                ),
                span,
            )),
        }
    }

    fn compare(&self, op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, Signal> {
        use std::cmp::Ordering::*;
        use BinOp::*;
        let ord = match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Str(a), Value::Str(b)) => a.partial_cmp(b),
            _ => {
                return Err(Signal::Error(
                    format!("cannot compare {} and {}", l.type_name(), r.type_name()),
                    span,
                ))
            }
        };
        let ord = ord.ok_or_else(|| Signal::Error("incomparable values".into(), span))?;
        let res = match op {
            Lt => ord == Less,
            Le => ord != Greater,
            Gt => ord == Greater,
            Ge => ord != Less,
            _ => false,
        };
        Ok(Value::Bool(res))
    }

    fn eval_call(
        &self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
        env: &mut Env,
    ) -> Result<Value, Signal> {
        if let Expr::Ident(fname, _) = callee {
            if fname == "to_string" {
                let v = self.eval_expr(&args[0], env)?;
                return Ok(Value::Str(display_plain(&v)));
            }
            if let Some(f) = self.funcs.get(fname).copied() {
                let mut argv = Vec::new();
                for a in args {
                    argv.push(self.eval_expr(a, env)?);
                }
                return self.call_fn(f, argv);
            }
            return Err(Signal::Error(format!("unknown function `{}`", fname), span));
        }
        Err(Signal::Error("cannot call this expression".into(), span))
    }

    fn eval_method(
        &self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        span: Span,
        env: &mut Env,
    ) -> Result<Value, Signal> {
        // A builtin module call: receiver is a module ident not shadowed locally.
        if let Expr::Ident(mod_name, _) = recv {
            if builtins::is_module(mod_name) && !env.has(mod_name) {
                let mut argv = Vec::new();
                for a in args {
                    argv.push(self.eval_expr(a, env)?);
                }
                return self.call_builtin(mod_name, name, argv, span);
            }
        }
        let r = self.eval_expr(recv, env)?;
        match name {
            "is_ok" => Ok(Value::Bool(r.is_ok())),
            "is_err" => Ok(Value::Bool(r.is_err())),
            "unwrap" => match r {
                Value::Ok(v) => Ok(*v),
                Value::Err(e) => Err(Signal::Error(format!("called unwrap on Err({})", e), span)),
                other => Err(Signal::Error(
                    format!("unwrap on non-Result {}", other.type_name()),
                    span,
                )),
            },
            "unwrap_err" => match r {
                Value::Err(v) => Ok(*v),
                _ => Err(Signal::Error("unwrap_err on non-Err".into(), span)),
            },
            _ => Err(Signal::Error(
                format!("unknown method `{}` on {}", name, r.type_name()),
                span,
            )),
        }
    }

    fn call_builtin(
        &self,
        module: &str,
        member: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, Signal> {
        match (module, member) {
            ("db", "seed") => {
                let key = args.first().map(|v| v.key()).unwrap_or_default();
                let row = args.get(1).cloned().unwrap_or(Value::Unit);
                self.db.borrow_mut().insert(key, row);
                Ok(Value::Unit)
            }
            ("db", "query") => {
                let key = args.get(1).map(|v| v.key()).unwrap_or_default();
                match self.db.borrow().get(&key) {
                    Some(row) => Ok(Value::Ok(Box::new(row.clone()))),
                    None => Ok(Value::Err(Box::new(Value::Str(format!(
                        "no row for key {}",
                        key
                    ))))),
                }
            }
            ("db", "exec") => Ok(Value::Unit),
            ("time", "now") => Ok(Value::Int(self.now)),
            ("log", "info") | ("log", "warn") => {
                let msg = args.first().map(display_plain).unwrap_or_default();
                self.log.borrow_mut().push(msg);
                Ok(Value::Unit)
            }
            ("net", "get") | ("net", "post") => Ok(Value::Ok(Box::new(Value::Str(
                "<network disabled in sandbox>".into(),
            )))),
            ("math", "abs") => match args.first() {
                Some(Value::Int(n)) => Ok(Value::Int(n.abs())),
                _ => Ok(Value::Int(0)),
            },
            ("math", "max") => Ok(int_fold(&args, true)),
            ("math", "min") => Ok(int_fold(&args, false)),
            _ => Err(Signal::Error(
                format!("unknown builtin `{}.{}`", module, member),
                span,
            )),
        }
    }
}

fn display_plain(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => format!("{}", other),
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Bool(x), Bool(y)) => x == y,
        (Str(x), Str(y)) => x == y,
        (Unit, Unit) => true,
        (Record(x), Record(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| value_eq(v, w)))
        }
        (Ok(x), Ok(y)) | (Err(x), Err(y)) => value_eq(x, y),
        (Variant(x), Variant(y)) => x == y,
        _ => false,
    }
}

/// Does a payload-less pattern match a value? `_` matches anything; a named
/// variant matches a `Variant` with the same name.
fn pattern_matches(pat: &Pattern, v: &Value) -> bool {
    match pat {
        Pattern::Wildcard { .. } => true,
        Pattern::Variant { name, .. } => matches!(v, Value::Variant(vn) if vn == name),
    }
}

fn int_fold(args: &[Value], max: bool) -> Value {
    let mut acc: Option<i64> = None;
    for a in args {
        if let Value::Int(n) = a {
            acc = Some(match acc {
                None => *n,
                Some(c) => {
                    if max {
                        c.max(*n)
                    } else {
                        c.min(*n)
                    }
                }
            });
        }
    }
    Value::Int(acc.unwrap_or(0))
}

fn binop_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

/// Render an expression back to readable Tach source — used for `ensure` failure
/// messages so the report reads like the code the author wrote.
pub fn render_expr(e: &Expr) -> String {
    match e {
        Expr::Int(n, _) => n.to_string(),
        Expr::Float(x, _) => x.to_string(),
        Expr::Str(s, _) => format!("\"{}\"", s),
        Expr::Bool(b, _) => b.to_string(),
        Expr::Ident(n, _) => n.clone(),
        Expr::Unary { op, expr, .. } => {
            let o = match op {
                UnOp::Not => "!",
                UnOp::Neg => "-",
            };
            format!("{}{}", o, render_expr(expr))
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            format!(
                "{} {} {}",
                render_expr(lhs),
                binop_str(*op),
                render_expr(rhs)
            )
        }
        Expr::Call { callee, args, .. } => {
            let a: Vec<String> = args.iter().map(render_expr).collect();
            format!("{}({})", render_expr(callee), a.join(", "))
        }
        Expr::Method {
            recv, name, args, ..
        } => {
            let a: Vec<String> = args.iter().map(render_expr).collect();
            format!("{}.{}({})", render_expr(recv), name, a.join(", "))
        }
        Expr::Field { recv, name, .. } => format!("{}.{}", render_expr(recv), name),
        Expr::Try { expr, .. } => format!("{}?", render_expr(expr)),
        Expr::Record { name, fields, .. } => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(n, e)| format!("{}: {}", n, render_expr(e)))
                .collect();
            let prefix = name.clone().map(|n| format!("{} ", n)).unwrap_or_default();
            format!("{}{{ {} }}", prefix, inner.join(", "))
        }
        Expr::Ok(e, _) => format!("Ok({})", render_expr(e)),
        Expr::Err(e, _) => format!("Err({})", render_expr(e)),
        Expr::Match {
            scrutinee, arms, ..
        } => {
            let a: Vec<String> = arms
                .iter()
                .map(|arm| {
                    format!(
                        "{} => {}",
                        pattern_str(&arm.pattern),
                        render_expr(&arm.body)
                    )
                })
                .collect();
            format!("match {} {{ {} }}", render_expr(scrutinee), a.join(", "))
        }
    }
}

fn pattern_str(p: &Pattern) -> String {
    match p {
        Pattern::Variant { name, .. } => name.clone(),
        Pattern::Wildcard { .. } => "_".into(),
    }
}
