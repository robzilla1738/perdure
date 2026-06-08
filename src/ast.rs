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
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Name { span, .. } => *span,
            TypeExpr::Record { span, .. } => *span,
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
            | Expr::Record { span, .. } => *span,
        }
    }
}
