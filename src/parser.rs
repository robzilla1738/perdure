use crate::ast::*;
use crate::diagnostics::Diagnostic;
use crate::lexer::lex;
use crate::span::Span;
use crate::token::{Tok, Token};

/// Recursive-descent parser with a Pratt expression core.
///
/// The grammar is deliberately small and has one obvious way to write things —
/// that is a feature, not a limitation. Fewer ambiguities means an agent editing
/// Tach has fewer ways to produce something that parses but means the wrong thing.
pub struct Parser {
    file: String,
    toks: Vec<Token>,
    pos: usize,
    diags: Vec<Diagnostic>,
    /// When true (inside `if`/`ensure` conditions), a bare `Name {` is NOT read as
    /// a record literal, so the `{` can open a block.
    no_record_lit: bool,
}

/// Parse a source file into a `Module` plus any diagnostics (lex + parse).
pub fn parse(file: &str, src: &str) -> (Module, Vec<Diagnostic>) {
    let (toks, lex_diags) = lex(file, src);
    let mut p = Parser {
        file: file.to_string(),
        toks,
        pos: 0,
        diags: lex_diags,
        no_record_lit: false,
    };
    let module = p.parse_module();
    (module, p.diags)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].kind
    }

    fn peek_at(&self, k: usize) -> &Tok {
        let i = (self.pos + k).min(self.toks.len() - 1);
        &self.toks[i].kind
    }

    fn peek_span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, k: &Tok) -> bool {
        if self.peek() == k {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, k: Tok) -> Span {
        if *self.peek() == k {
            self.bump().span
        } else {
            let found = self.peek().human();
            let sp = self.peek_span();
            self.push_err(
                "E0003",
                "syntax",
                format!("expected {}, found {}", k.human(), found),
                sp,
            );
            sp
        }
    }

    fn expect_ident(&mut self) -> (String, Span) {
        if let Tok::Ident(name) = self.peek().clone() {
            let sp = self.peek_span();
            self.bump();
            (name, sp)
        } else {
            let found = self.peek().human();
            let sp = self.peek_span();
            self.push_err(
                "E0003",
                "syntax",
                format!("expected an identifier, found {}", found),
                sp,
            );
            self.bump();
            ("<error>".into(), sp)
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.bump();
        }
    }

    fn push_err(&mut self, code: &str, kind: &str, msg: String, sp: Span) {
        let file = self.file.clone();
        self.diags
            .push(Diagnostic::error(code, kind, msg, &file, sp));
    }

    fn error_here(&mut self, msg: &str) {
        let found = self.peek().human();
        let sp = self.peek_span();
        self.push_err("E0003", "syntax", format!("{}, found {}", msg, found), sp);
    }

    // ----- top level -----

    fn parse_module(&mut self) -> Module {
        let mut items = Vec::new();
        let mut last_import_end = 0usize;
        self.skip_newlines();
        while !matches!(self.peek(), Tok::Eof) {
            let before = self.pos;
            match self.peek() {
                Tok::Import => {
                    let im = self.parse_import();
                    last_import_end = last_import_end.max(im.span.end);
                    items.push(Item::Import(im));
                }
                Tok::Type => items.push(Item::Type(self.parse_type_decl())),
                Tok::Fn => items.push(Item::Fn(self.parse_fn())),
                Tok::Test => items.push(Item::Test(self.parse_test())),
                _ => {
                    self.error_here("expected a top-level item (import, type, fn, or test)");
                    self.bump();
                }
            }
            if self.pos == before {
                self.bump();
            }
            self.skip_newlines();
        }
        Module {
            items,
            last_import_end,
        }
    }

    fn parse_import(&mut self) -> Import {
        let kw = self.expect(Tok::Import);
        let (module, sp) = self.expect_ident();
        Import {
            module,
            span: kw.to(sp),
        }
    }

    fn parse_type_decl(&mut self) -> TypeDecl {
        let kw = self.expect(Tok::Type);
        let (name, _) = self.expect_ident();
        self.expect(Tok::Eq);
        let ty = self.parse_type();
        let span = kw.to(ty.span());
        TypeDecl { name, ty, span }
    }

    fn parse_test(&mut self) -> TestDecl {
        let kw = self.expect(Tok::Test);
        let name = if let Tok::Str(s) = self.peek().clone() {
            self.bump();
            s
        } else {
            self.error_here("expected a test name string");
            "<unnamed>".into()
        };
        let body = self.parse_block();
        let span = kw.to(body.span);
        TestDecl { name, body, span }
    }

    fn parse_fn(&mut self) -> FnDecl {
        let kw = self.expect(Tok::Fn);
        let (name, name_span) = self.expect_ident();
        self.expect(Tok::LParen);
        let mut params = Vec::new();
        self.skip_newlines();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                let (pn, pnsp) = self.expect_ident();
                self.expect(Tok::Colon);
                let ty = self.parse_type();
                let pspan = pnsp.to(ty.span());
                params.push(Param {
                    name: pn,
                    ty,
                    span: pspan,
                });
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
        }
        self.expect(Tok::RParen);
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.parse_type())
        } else {
            None
        };
        let effects = if matches!(self.peek(), Tok::Effects) {
            Some(self.parse_effects_clause())
        } else {
            None
        };
        let brace_offset = self.peek_span().start;
        let body = self.parse_block();
        let span = kw.to(body.span);
        FnDecl {
            name,
            name_span,
            params,
            ret,
            effects,
            body,
            span,
            brace_offset,
        }
    }

    fn parse_effects_clause(&mut self) -> EffectsClause {
        let kw = self.expect(Tok::Effects);
        let lb = self.expect(Tok::LBracket);
        let mut effects = Vec::new();
        self.skip_newlines();
        if !matches!(self.peek(), Tok::RBracket) {
            loop {
                let er = self.parse_effect_ref();
                effects.push(er);
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
        }
        let rb = self.expect(Tok::RBracket);
        EffectsClause {
            effects,
            list_span: Span::new(lb.end, rb.start),
            full_span: kw.to(rb),
        }
    }

    fn parse_effect_ref(&mut self) -> EffectRef {
        let (first, sp0) = self.expect_ident();
        let mut name = first;
        let mut span = sp0;
        while matches!(self.peek(), Tok::Dot) {
            self.bump();
            let (seg, segsp) = self.expect_ident();
            name.push('.');
            name.push_str(&seg);
            span = span.to(segsp);
        }
        EffectRef { name, span }
    }

    fn parse_type(&mut self) -> TypeExpr {
        if matches!(self.peek(), Tok::LBrace) {
            let lb = self.expect(Tok::LBrace);
            let mut fields = Vec::new();
            loop {
                self.skip_newlines();
                if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    break;
                }
                let (fname, _) = self.expect_ident();
                self.expect(Tok::Colon);
                let fty = self.parse_type();
                fields.push((fname, fty));
                self.skip_newlines();
                self.eat(&Tok::Comma);
            }
            let rb = self.expect(Tok::RBrace);
            TypeExpr::Record {
                fields,
                span: lb.to(rb),
            }
        } else {
            let (name, sp) = self.expect_ident();
            let mut span = sp;
            let mut args = Vec::new();
            if matches!(self.peek(), Tok::Lt) {
                self.bump();
                loop {
                    let t = self.parse_type();
                    span = span.to(t.span());
                    args.push(t);
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
                let gt = self.expect(Tok::Gt);
                span = span.to(gt);
            }
            TypeExpr::Name { name, args, span }
        }
    }

    fn parse_block(&mut self) -> Block {
        let lb = self.expect(Tok::LBrace);
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            let before = self.pos;
            let s = self.parse_stmt();
            stmts.push(s);
            if self.pos == before {
                self.bump();
            }
        }
        let rb = self.expect(Tok::RBrace);
        Block {
            stmts,
            span: lb.to(rb),
        }
    }

    // ----- statements -----

    fn parse_stmt(&mut self) -> Stmt {
        match self.peek() {
            Tok::Let => {
                let sp = self.peek_span();
                self.bump();
                let (name, name_span) = self.expect_ident();
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type())
                } else {
                    None
                };
                self.expect(Tok::Eq);
                let value = self.parse_expr();
                let span = sp.to(value.span());
                Stmt::Let {
                    name,
                    name_span,
                    ty,
                    value,
                    span,
                }
            }
            Tok::Return => {
                let sp = self.peek_span();
                self.bump();
                if matches!(self.peek(), Tok::Newline | Tok::RBrace | Tok::Eof) {
                    Stmt::Return {
                        value: None,
                        span: sp,
                    }
                } else {
                    let e = self.parse_expr();
                    let span = sp.to(e.span());
                    Stmt::Return {
                        value: Some(e),
                        span,
                    }
                }
            }
            Tok::Ensure => {
                let sp = self.peek_span();
                self.bump();
                let saved = self.no_record_lit;
                self.no_record_lit = true;
                let cond = self.parse_expr();
                self.no_record_lit = saved;
                let els = if self.eat(&Tok::Else) {
                    Some(self.parse_expr())
                } else {
                    None
                };
                let end = els
                    .as_ref()
                    .map(|e| e.span())
                    .unwrap_or_else(|| cond.span());
                Stmt::Ensure {
                    cond,
                    els,
                    span: sp.to(end),
                }
            }
            Tok::If => {
                let sp = self.peek_span();
                self.bump();
                let saved = self.no_record_lit;
                self.no_record_lit = true;
                let cond = self.parse_expr();
                self.no_record_lit = saved;
                let then = self.parse_block();
                let els = if self.eat(&Tok::Else) {
                    Some(self.parse_block())
                } else {
                    None
                };
                let end = els.as_ref().map(|b| b.span).unwrap_or(then.span);
                Stmt::If {
                    cond,
                    then,
                    els,
                    span: sp.to(end),
                }
            }
            _ => Stmt::Expr(self.parse_expr()),
        }
    }

    // ----- expressions (Pratt by precedence climbing) -----

    fn parse_expr(&mut self) -> Expr {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Expr {
        let mut e = self.parse_and();
        while matches!(self.peek(), Tok::OrOr) {
            self.bump();
            let rhs = self.parse_and();
            let span = e.span().to(rhs.span());
            e = Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
                span,
            };
        }
        e
    }

    fn parse_and(&mut self) -> Expr {
        let mut e = self.parse_cmp();
        while matches!(self.peek(), Tok::AndAnd) {
            self.bump();
            let rhs = self.parse_cmp();
            let span = e.span().to(rhs.span());
            e = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
                span,
            };
        }
        e
    }

    fn parse_cmp(&mut self) -> Expr {
        let mut e = self.parse_add();
        loop {
            let op = match self.peek() {
                Tok::EqEq => BinOp::Eq,
                Tok::NotEq => BinOp::Ne,
                Tok::Lt => BinOp::Lt,
                Tok::LtEq => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::GtEq => BinOp::Ge,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_add();
            let span = e.span().to(rhs.span());
            e = Expr::Binary {
                op,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
                span,
            };
        }
        e
    }

    fn parse_add(&mut self) -> Expr {
        let mut e = self.parse_mul();
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul();
            let span = e.span().to(rhs.span());
            e = Expr::Binary {
                op,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
                span,
            };
        }
        e
    }

    fn parse_mul(&mut self) -> Expr {
        let mut e = self.parse_unary();
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary();
            let span = e.span().to(rhs.span());
            e = Expr::Binary {
                op,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
                span,
            };
        }
        e
    }

    fn parse_unary(&mut self) -> Expr {
        match self.peek() {
            Tok::Bang => {
                let sp = self.peek_span();
                self.bump();
                let e = self.parse_unary();
                let span = sp.to(e.span());
                Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                    span,
                }
            }
            Tok::Minus => {
                let sp = self.peek_span();
                self.bump();
                let e = self.parse_unary();
                let span = sp.to(e.span());
                Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(e),
                    span,
                }
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Expr {
        let mut e = self.parse_primary();
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.bump();
                    let (name, name_span) = self.expect_ident();
                    if matches!(self.peek(), Tok::LParen) {
                        let (args, end) = self.parse_args();
                        let span = e.span().to(end);
                        e = Expr::Method {
                            recv: Box::new(e),
                            name,
                            name_span,
                            args,
                            span,
                        };
                    } else {
                        let span = e.span().to(name_span);
                        e = Expr::Field {
                            recv: Box::new(e),
                            name,
                            name_span,
                            span,
                        };
                    }
                }
                Tok::LParen => {
                    let (args, end) = self.parse_args();
                    let span = e.span().to(end);
                    e = Expr::Call {
                        callee: Box::new(e),
                        args,
                        span,
                    };
                }
                Tok::Question => {
                    let qsp = self.peek_span();
                    self.bump();
                    let span = e.span().to(qsp);
                    e = Expr::Try {
                        expr: Box::new(e),
                        span,
                    };
                }
                _ => break,
            }
        }
        e
    }

    fn parse_args(&mut self) -> (Vec<Expr>, Span) {
        self.expect(Tok::LParen);
        let saved = self.no_record_lit;
        self.no_record_lit = false;
        let mut args = Vec::new();
        self.skip_newlines();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                args.push(self.parse_expr());
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
        }
        let end = self.expect(Tok::RParen);
        self.no_record_lit = saved;
        (args, end)
    }

    fn parse_primary(&mut self) -> Expr {
        let sp = self.peek_span();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.bump();
                Expr::Int(v, sp)
            }
            Tok::Float(v) => {
                self.bump();
                Expr::Float(v, sp)
            }
            Tok::Str(s) => {
                self.bump();
                Expr::Str(s, sp)
            }
            Tok::True => {
                self.bump();
                Expr::Bool(true, sp)
            }
            Tok::False => {
                self.bump();
                Expr::Bool(false, sp)
            }
            Tok::Ok => {
                self.bump();
                let inner = self.parse_paren_expr();
                let span = sp.to(inner.1);
                Expr::Ok(Box::new(inner.0), span)
            }
            Tok::Err => {
                self.bump();
                let inner = self.parse_paren_expr();
                let span = sp.to(inner.1);
                Expr::Err(Box::new(inner.0), span)
            }
            Tok::LParen => {
                self.bump();
                let saved = self.no_record_lit;
                self.no_record_lit = false;
                let e = self.parse_expr();
                self.no_record_lit = saved;
                self.expect(Tok::RParen);
                e
            }
            Tok::LBrace if !self.no_record_lit => self.parse_record_lit(None, sp),
            Tok::Ident(name) => {
                if !self.no_record_lit && matches!(self.peek_at(1), Tok::LBrace) {
                    self.bump();
                    self.parse_record_lit(Some(name), sp)
                } else {
                    self.bump();
                    Expr::Ident(name, sp)
                }
            }
            _ => {
                self.error_here("expected an expression");
                self.bump();
                Expr::Ident("<error>".into(), sp)
            }
        }
    }

    /// Parse `( expr )` returning the inner expression and the closing-paren span.
    fn parse_paren_expr(&mut self) -> (Expr, Span) {
        self.expect(Tok::LParen);
        let saved = self.no_record_lit;
        self.no_record_lit = false;
        let e = self.parse_expr();
        let end = self.expect(Tok::RParen);
        self.no_record_lit = saved;
        (e, end)
    }

    fn parse_record_lit(&mut self, name: Option<String>, start: Span) -> Expr {
        self.expect(Tok::LBrace);
        let saved = self.no_record_lit;
        self.no_record_lit = false;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            let (fname, _) = self.expect_ident();
            self.expect(Tok::Colon);
            let value = self.parse_expr();
            fields.push((fname, value));
            self.skip_newlines();
            if self.eat(&Tok::Comma) {
                self.skip_newlines();
                continue;
            }
            break;
        }
        self.skip_newlines();
        let rb = self.expect(Tok::RBrace);
        self.no_record_lit = saved;
        Expr::Record {
            name,
            fields,
            span: start.to(rb),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
import db

type Session = {
  token: String
  expires_at: Int
}

fn load_session(token: String) -> Result<Session, AuthError> {
  let row = db.query("select * from sessions where token = ?", token)?
  ensure row.expires_at > time.now()
  return Ok(Session { token: row.token, expires_at: row.expires_at })
}

test "loads" {
  ensure load_session("x").is_err()
}
"#;

    #[test]
    fn parses_sample_cleanly() {
        let (module, diags) = parse("sample.tach", SAMPLE);
        let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "unexpected parse errors: {:?}", errs);
        assert_eq!(module.items.len(), 4);

        // The fn should have no effects clause yet and a real brace offset.
        let f = module.items.iter().find_map(|i| match i {
            Item::Fn(f) if f.name == "load_session" => Some(f),
            _ => None,
        });
        let f = f.expect("load_session fn parsed");
        assert!(f.effects.is_none());
        assert!(f.brace_offset > 0);
        assert!(f.ret.is_some());
        assert_eq!(f.body.stmts.len(), 3);
    }
}
