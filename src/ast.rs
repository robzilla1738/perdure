use crate::span::Span;

/// A whole parsed source file.
#[derive(Clone, Debug)]
pub struct Module {
    pub items: Vec<Item>,
    /// Byte offset just after the last `import` statement (insertion point for a
    /// new import), or 0 if there are none.
    pub last_import_end: usize,
}

#[derive(Clone, Debug)]
pub enum Item {
    Import(Import),
    Type(TypeDecl),
    Fn(FnDecl),
    Test(TestDecl),
    Goal(GoalDecl),
}

/// A `goal` declaration: the typed, durable contract a long-horizon agent run is
/// held to. A goal is declarative — it names the budget it may spend, the
/// authority it is allowed to exercise (effects, file scopes, tools), and the
/// conditions that must hold for it to be considered a success. The durable
/// runtime (`tach goal run`) executes the repair loop under exactly these
/// constraints, checkpointing as it goes so a crashed run can resume without
/// repeating work.
#[derive(Clone, Debug)]
pub struct GoalDecl {
    pub name: String,
    pub name_span: Span,
    /// The success type after `->`, if written (e.g. `-> Success`). Documentary.
    pub success: Option<String>,
    pub budget: GoalBudget,
    pub allow: GoalAllow,
    pub require: GoalRequire,
    /// The optional `plan { ... }` block: a durable, re-executable workflow of
    /// tool calls, approval gates, and control flow. When present the goal is an
    /// *action* goal driven by the plan interpreter rather than the repair loop.
    pub plan: Option<PlanBlock>,
    pub span: Span,
}

/// The body of a `plan { ... }` block — a long-horizon agentic workflow.
///
/// Unlike a `fn` body (statically typed, pure computation), plan statements
/// *orchestrate*: they call (fake, offline) tools, pause at human approval
/// gates, and branch/loop over tool output. The runtime re-executes the whole
/// block from the top on every resume, memoizing already-completed calls by
/// their durable receipt — so loops and crash/resume produce each side effect
/// exactly once. Expressions reuse the ordinary `Expr` grammar; only the
/// statement forms (`call`, `approve`, `for`, `while`) are plan-specific.
#[derive(Clone, Debug)]
pub struct PlanBlock {
    pub stmts: Vec<PlanStmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PlanStmt {
    /// `let name = <value>` — bind or rebind a name in the plan's single scope.
    Let {
        name: String,
        name_span: Span,
        value: PlanValue,
        span: Span,
    },
    /// A bare tool call whose output is discarded, e.g. `call fake.email.send { ... }`.
    Call { call: PlanCall, span: Span },
    /// `approve "summary" { body }` — a human approval gate over a sub-plan. The
    /// body runs only after a `tach goal approve` grants the gate.
    Approve {
        summary: String,
        body: Vec<PlanStmt>,
        span: Span,
    },
    /// `if cond { ... } else { ... }` over a boolean tool/field value.
    If {
        cond: Expr,
        then: Vec<PlanStmt>,
        els: Option<Vec<PlanStmt>>,
        span: Span,
    },
    /// `for var in <array-expr> { ... }` — iterate a JSON array (typically a tool's output).
    For {
        var: String,
        var_span: Span,
        iter: Expr,
        body: Vec<PlanStmt>,
        span: Span,
    },
    /// `while cond { ... }` — repeat while a boolean expression holds (bounded by budget).
    While {
        cond: Expr,
        body: Vec<PlanStmt>,
        span: Span,
    },
}

/// The right-hand side of a plan `let`: either a tool call or a pure expression.
#[derive(Clone, Debug)]
pub enum PlanValue {
    Call(PlanCall),
    Expr(Expr),
}

/// A `call <tool> { key: expr, ... }` tool invocation. The `input` fields are
/// evaluated to a JSON object the runtime feeds to the (fake) tool.
#[derive(Clone, Debug)]
pub struct PlanCall {
    pub tool: String,
    pub tool_span: Span,
    pub input: Vec<(String, Expr)>,
    pub span: Span,
}

/// The `budget { ... }` block: the resources a goal run may spend before it is
/// declared exhausted. `steps` and `retries` are enforced deterministically;
/// `time` and `cost` are recorded (and surfaced in the trace) but, being
/// wall-clock and accounting concerns, are not part of the replayable core.
#[derive(Clone, Debug, Default)]
pub struct GoalBudget {
    pub steps: Option<u64>,
    pub retries: Option<u64>,
    /// Raw duration text exactly as written, e.g. `20m`.
    pub time: Option<String>,
    pub cost: Option<i64>,
    pub span: Span,
}

/// The `allow { ... }` block: the authority a goal run is granted. A run may
/// perform only the effects listed here, may write only files matching
/// `fs_write`, and may invoke only the listed tools/shell commands. The runtime
/// rejects any patch that would exceed this surface — before it touches disk.
#[derive(Clone, Debug, Default)]
pub struct GoalAllow {
    pub effects: Vec<EffectRef>,
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub shell: Vec<String>,
    pub tools: Vec<String>,
    pub span: Span,
}

/// The `require { ... }` block: the conditions that must hold for the run to be
/// considered a success (e.g. `tests.pass`, `no_new_effects`).
#[derive(Clone, Debug, Default)]
pub struct GoalRequire {
    pub conditions: Vec<RequireCond>,
    pub span: Span,
}

/// One success condition, carrying its span so the checker can point at it.
#[derive(Clone, Debug)]
pub struct RequireCond {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Import {
    pub module: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeExpr {
    /// A named (possibly generic) type, e.g. `Int`, `UserId`, `Result<User, ApiError>`.
    Name {
        name: String,
        args: Vec<TypeExpr>,
        span: Span,
    },
    /// An anonymous record type, e.g. `{ id: Int, name: String }`.
    Record {
        fields: Vec<(String, TypeExpr)>,
        span: Span,
    },
    /// A sum type — a set of payload-less variants, e.g. `Red | Green | Blue`.
    Sum { variants: Vec<Variant>, span: Span },
}

/// One variant of a sum type. Payload-less for now.
#[derive(Clone, Debug)]
pub struct Variant {
    pub name: String,
    pub span: Span,
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Name { span, .. } => *span,
            TypeExpr::Record { span, .. } => *span,
            TypeExpr::Sum { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

/// A single declared effect like `db.read` or `stripe.refund`.
#[derive(Clone, Debug)]
pub struct EffectRef {
    pub name: String,
    pub span: Span,
}

/// The `effects [...]` clause of a function signature, with enough span detail
/// to either rewrite the list in place or insert a fresh clause.
#[derive(Clone, Debug)]
pub struct EffectsClause {
    pub effects: Vec<EffectRef>,
    /// Span covering the contents between the brackets (what to rewrite when
    /// adding an effect to an existing list).
    pub list_span: Span,
    /// Span covering the whole `effects [...]` clause.
    pub full_span: Span,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub effects: Option<EffectsClause>,
    pub body: Block,
    pub span: Span,
    /// Byte offset of the body's opening `{` — the insertion point for a new
    /// `effects [...]` clause when one is missing.
    pub brace_offset: usize,
}

#[derive(Clone, Debug)]
pub struct TestDecl {
    pub name: String,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let {
        name: String,
        name_span: Span,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    Ensure {
        cond: Expr,
        els: Option<Expr>,
        span: Span,
    },
    If {
        cond: Expr,
        then: Block,
        els: Option<Block>,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Bool(bool, Span),
    Ident(String, Span),
    Unary {
        op: UnOp,
        expr: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    Method {
        recv: Box<Expr>,
        name: String,
        name_span: Span,
        args: Vec<Expr>,
        span: Span,
    },
    Field {
        recv: Box<Expr>,
        name: String,
        name_span: Span,
        span: Span,
    },
    Try {
        expr: Box<Expr>,
        span: Span,
    },
    Record {
        name: Option<String>,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    Ok(Box<Expr>, Span),
    Err(Box<Expr>, Span),
    /// `match scrutinee { pattern => body, ... }`.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
}

/// One arm of a `match`: a pattern and the expression it evaluates to.
#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

/// A `match` pattern. Payload-less: a named variant, or the `_` catch-all.
#[derive(Clone, Debug)]
pub enum Pattern {
    Variant { name: String, span: Span },
    Wildcard { span: Span },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Variant { span, .. } => *span,
            Pattern::Wildcard { span } => *span,
        }
    }
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Str(_, s)
            | Expr::Bool(_, s)
            | Expr::Ident(_, s)
            | Expr::Ok(_, s)
            | Expr::Err(_, s) => *s,
            Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Method { span, .. }
            | Expr::Field { span, .. }
            | Expr::Try { span, .. }
            | Expr::Record { span, .. }
            | Expr::Match { span, .. } => *span,
        }
    }
}
