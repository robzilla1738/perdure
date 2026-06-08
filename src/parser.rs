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
                Tok::Goal => items.push(Item::Goal(self.parse_goal())),
                _ => {
                    self.error_here("expected a top-level item (import, type, fn, test, or goal)");
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
        // A sum type is a run of bare variant names joined by `|`. We only treat
        // the RHS as a sum when an identifier is immediately followed by `|`;
        // otherwise it is a record or a (possibly generic) named type.
        let ty = if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Pipe) {
            let (first, fsp) = self.expect_ident();
            let mut variants = vec![Variant {
                name: first,
                span: fsp,
            }];
            let mut span = fsp;
            while self.eat(&Tok::Pipe) {
                let (vn, vsp) = self.expect_ident();
                span = span.to(vsp);
                variants.push(Variant {
                    name: vn,
                    span: vsp,
                });
            }
            TypeExpr::Sum { variants, span }
        } else {
            self.parse_type()
        };
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

    // ----- goals -----

    /// Parse a dotted name like `db.read`, `fs.write`, or `tach.check` into a
    /// single string. Goals describe authority over namespaced tools and effects,
    /// so a dotted path is the natural identifier there.
    fn parse_dotted_name(&mut self) -> (String, Span) {
        let (first, sp0) = self.expect_ident();
        let mut name = first;
        let mut span = sp0;
        while matches!(self.peek(), Tok::Dot) {
            self.bump();
            let (seg, sp) = self.expect_ident();
            name.push('.');
            name.push_str(&seg);
            span = span.to(sp);
        }
        (name, span)
    }

    /// One or more glob patterns: either a bare `"glob"` or a `["a", "b"]` list.
    fn parse_glob_values(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if matches!(self.peek(), Tok::LBracket) {
            self.bump();
            self.skip_newlines();
            while !matches!(self.peek(), Tok::RBracket | Tok::Eof) {
                if let Tok::Str(s) = self.peek().clone() {
                    self.bump();
                    out.push(s);
                } else {
                    self.error_here("expected a string glob");
                    self.bump();
                }
                self.skip_newlines();
                if self.eat(&Tok::Comma) {
                    self.skip_newlines();
                }
            }
            self.expect(Tok::RBracket);
        } else if let Tok::Str(s) = self.peek().clone() {
            self.bump();
            out.push(s);
        } else {
            self.error_here("expected a glob string or a `[...]` list");
        }
        out
    }

    fn parse_goal(&mut self) -> GoalDecl {
        let kw = self.expect(Tok::Goal);
        let (name, name_span) = self.expect_ident();
        let success = if self.eat(&Tok::Arrow) {
            Some(self.expect_ident().0)
        } else {
            None
        };
        self.expect(Tok::LBrace);
        self.skip_newlines();
        let mut budget = GoalBudget::default();
        let mut allow = GoalAllow::default();
        let mut require = GoalRequire::default();
        let mut plan = None;
        while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
            let before = self.pos;
            if let Tok::Ident(section) = self.peek().clone() {
                let ssp = self.peek_span();
                self.bump();
                match section.as_str() {
                    "budget" => budget = self.parse_goal_budget(ssp),
                    "allow" => allow = self.parse_goal_allow(ssp),
                    "require" => require = self.parse_goal_require(ssp),
                    "plan" => plan = Some(self.parse_goal_plan(ssp)),
                    other => {
                        self.push_err(
                            "E0003",
                            "syntax",
                            format!(
                                "unknown goal section `{}` (expected `budget`, `allow`, `require`, or `plan`)",
                                other
                            ),
                            ssp,
                        );
                        self.skip_brace_block();
                    }
                }
            } else {
                self.error_here(
                    "expected a goal section (`budget`, `allow`, `require`, or `plan`)",
                );
            }
            if self.pos == before {
                self.bump();
            }
            self.skip_newlines();
        }
        let close = self.expect(Tok::RBrace);
        GoalDecl {
            name,
            name_span,
            success,
            budget,
            allow,
            require,
            plan,
            span: kw.to(close),
        }
    }

    fn parse_goal_budget(&mut self, kw: Span) -> GoalBudget {
        let mut b = GoalBudget {
            span: kw,
            ..Default::default()
        };
        self.expect(Tok::LBrace);
        self.skip_newlines();
        while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
            let before = self.pos;
            let (key, ksp) = self.expect_ident();
            self.expect(Tok::Colon);
            b.span = b.span.to(ksp);
            match key.as_str() {
                "steps" => b.steps = Some(self.parse_uint()),
                "retries" => b.retries = Some(self.parse_uint()),
                "cost" => b.cost = Some(self.parse_signed_int()),
                "time" => b.time = Some(self.parse_duration_text()),
                other => {
                    self.push_err(
                        "E0003",
                        "syntax",
                        format!(
                            "unknown budget key `{}` (expected `steps`, `retries`, `time`, or `cost`)",
                            other
                        ),
                        ksp,
                    );
                    // consume the rest of the line so we can recover.
                    while !matches!(self.peek(), Tok::Newline | Tok::RBrace | Tok::Eof) {
                        self.bump();
                    }
                }
            }
            if self.pos == before {
                self.bump();
            }
            self.skip_newlines();
        }
        self.expect(Tok::RBrace);
        b
    }

    fn parse_goal_allow(&mut self, kw: Span) -> GoalAllow {
        let mut a = GoalAllow {
            span: kw,
            ..Default::default()
        };
        self.expect(Tok::LBrace);
        self.skip_newlines();
        while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
            let before = self.pos;
            let (head, hsp) = self.parse_dotted_name();
            a.span = a.span.to(hsp);
            match head.as_str() {
                "effect" => {
                    // `effect db.read`
                    let (eff, esp) = self.parse_dotted_name();
                    a.effects.push(EffectRef {
                        name: eff,
                        span: esp,
                    });
                }
                "fs.read" => a.fs_read.extend(self.parse_glob_values()),
                "fs.write" => a.fs_write.extend(self.parse_glob_values()),
                "shell.run" => a.shell.extend(self.parse_glob_values()),
                _ => {
                    // A bare dotted name is a tool grant, e.g. `tach.check`.
                    a.tools.push(head);
                }
            }
            if self.pos == before {
                self.bump();
            }
            self.skip_newlines();
        }
        self.expect(Tok::RBrace);
        a
    }

    fn parse_goal_require(&mut self, kw: Span) -> GoalRequire {
        let mut r = GoalRequire {
            span: kw,
            ..Default::default()
        };
        self.expect(Tok::LBrace);
        self.skip_newlines();
        while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
            let before = self.pos;
            let (name, sp) = self.parse_dotted_name();
            r.span = r.span.to(sp);
            r.conditions.push(RequireCond { name, span: sp });
            if self.pos == before {
                self.bump();
            }
            self.skip_newlines();
        }
        self.expect(Tok::RBrace);
        r
    }

    // ----- plan blocks -----

    /// True when the current token is the identifier `kw` (a contextual keyword
    /// like `call`/`approve`/`for`/`while`/`in`, none of which the lexer reserves).
    fn peek_ident_is(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s.as_str() == kw)
    }

    /// `plan { <plan-stmts> }` — the durable workflow body of an action goal.
    fn parse_goal_plan(&mut self, kw: Span) -> PlanBlock {
        let (stmts, bspan) = self.parse_plan_body();
        PlanBlock {
            stmts,
            span: kw.to(bspan),
        }
    }

    /// A brace-delimited run of plan statements; returns the statements and the
    /// span of the whole `{ ... }`. Shared by the top-level plan and every nested
    /// body (`if`/`else`/`for`/`while`/`approve`).
    fn parse_plan_body(&mut self) -> (Vec<PlanStmt>, Span) {
        let lb = self.expect(Tok::LBrace);
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            let before = self.pos;
            if let Some(s) = self.parse_plan_stmt() {
                stmts.push(s);
            }
            if self.pos == before {
                self.bump();
            }
        }
        let rb = self.expect(Tok::RBrace);
        (stmts, lb.to(rb))
    }

    fn parse_plan_stmt(&mut self) -> Option<PlanStmt> {
        let sp = self.peek_span();
        match self.peek().clone() {
            Tok::Let => {
                self.bump();
                let (name, name_span) = self.expect_ident();
                self.expect(Tok::Eq);
                let value = self.parse_plan_value();
                let end = match &value {
                    PlanValue::Call(c) => c.span,
                    PlanValue::Expr(e) => e.span(),
                };
                Some(PlanStmt::Let {
                    name,
                    name_span,
                    value,
                    span: sp.to(end),
                })
            }
            Tok::If => {
                self.bump();
                let saved = self.no_record_lit;
                self.no_record_lit = true;
                let cond = self.parse_expr();
                self.no_record_lit = saved;
                let (then, tspan) = self.parse_plan_body();
                // Allow `else` on the line after the `}` (newlines are separators,
                // so without this an `else` on its own line would detach).
                self.skip_newlines();
                let (els, end) = if self.eat(&Tok::Else) {
                    let (eb, espan) = self.parse_plan_body();
                    (Some(eb), espan)
                } else {
                    (None, tspan)
                };
                Some(PlanStmt::If {
                    cond,
                    then,
                    els,
                    span: sp.to(end),
                })
            }
            Tok::Ident(ref kw) if kw == "for" => {
                self.bump();
                let (var, var_span) = self.expect_ident();
                if self.peek_ident_is("in") {
                    self.bump();
                } else {
                    self.error_here("expected `in` after the loop variable");
                }
                let saved = self.no_record_lit;
                self.no_record_lit = true;
                let iter = self.parse_expr();
                self.no_record_lit = saved;
                let (body, bspan) = self.parse_plan_body();
                Some(PlanStmt::For {
                    var,
                    var_span,
                    iter,
                    body,
                    span: sp.to(bspan),
                })
            }
            Tok::Ident(ref kw) if kw == "while" => {
                self.bump();
                let saved = self.no_record_lit;
                self.no_record_lit = true;
                let cond = self.parse_expr();
                self.no_record_lit = saved;
                let (body, bspan) = self.parse_plan_body();
                Some(PlanStmt::While {
                    cond,
                    body,
                    span: sp.to(bspan),
                })
            }
            Tok::Ident(ref kw) if kw == "approve" => {
                self.bump();
                let summary = if let Tok::Str(s) = self.peek().clone() {
                    self.bump();
                    s
                } else {
                    String::new()
                };
                let (body, bspan) = self.parse_plan_body();
                Some(PlanStmt::Approve {
                    summary,
                    body,
                    span: sp.to(bspan),
                })
            }
            Tok::Ident(ref kw) if kw == "call" => {
                let call = self.parse_plan_call();
                let span = sp.to(call.span);
                Some(PlanStmt::Call { call, span })
            }
            _ => {
                self.error_here(
                    "expected a plan statement (`let`, `call`, `approve`, `if`, `for`, or `while`)",
                );
                self.bump();
                None
            }
        }
    }

    /// The right-hand side of a plan `let`: a tool call or a pure expression.
    fn parse_plan_value(&mut self) -> PlanValue {
        if self.peek_ident_is("call") {
            PlanValue::Call(self.parse_plan_call())
        } else {
            PlanValue::Expr(self.parse_expr())
        }
    }

    /// `call <dotted.tool.name> { key: expr, ... }`.
    fn parse_plan_call(&mut self) -> PlanCall {
        let start = self.peek_span();
        self.bump(); // `call`
        let (tool, tool_span) = self.parse_dotted_name();
        let (input, end) = self.parse_plan_input();
        PlanCall {
            tool,
            tool_span,
            input,
            span: start.to(end),
        }
    }

    /// `{ key: expr, ... }` — the tool input record. Fields may be separated by
    /// commas or newlines (newlines are live inside `{}`), returning the parsed
    /// pairs and the closing-brace span.
    fn parse_plan_input(&mut self) -> (Vec<(String, Expr)>, Span) {
        // Tolerate a newline between the tool name and its input record, so
        // `call t\n{ ... }` parses (a `call` always has an input record).
        self.skip_newlines();
        self.expect(Tok::LBrace);
        let saved = self.no_record_lit;
        self.no_record_lit = false;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            let before = self.pos;
            let (key, _) = self.expect_ident();
            self.expect(Tok::Colon);
            let value = self.parse_expr();
            fields.push((key, value));
            self.skip_newlines();
            self.eat(&Tok::Comma);
            if self.pos == before {
                self.bump();
            }
        }
        let rb = self.expect(Tok::RBrace);
        self.no_record_lit = saved;
        (fields, rb)
    }

    /// Parse an unsigned integer literal, recovering to 0 on anything else.
    fn parse_uint(&mut self) -> u64 {
        if let Tok::Int(n) = self.peek().clone() {
            self.bump();
            n.max(0) as u64
        } else {
            self.error_here("expected a whole number");
            0
        }
    }

    fn parse_signed_int(&mut self) -> i64 {
        let neg = self.eat(&Tok::Minus);
        if let Tok::Int(n) = self.peek().clone() {
            self.bump();
            if neg {
                -n
            } else {
                n
            }
        } else {
            self.error_here("expected a whole number");
            0
        }
    }

    /// A duration like `20m` or `45` — an integer with an optional unit suffix
    /// identifier. Stored as raw text; the runtime parses it only when enforcing.
    fn parse_duration_text(&mut self) -> String {
        let n = self.parse_uint();
        let mut s = n.to_string();
        if let Tok::Ident(unit) = self.peek().clone() {
            self.bump();
            s.push_str(&unit);
        }
        s
    }

    /// Skip a balanced `{ ... }` block (used to recover from an unknown section).
    fn skip_brace_block(&mut self) {
        if !self.eat(&Tok::LBrace) {
            return;
        }
        let mut depth = 1;
        while depth > 0 && !matches!(self.peek(), Tok::Eof) {
            match self.peek() {
                Tok::LBrace => depth += 1,
                Tok::RBrace => depth -= 1,
                _ => {}
            }
            self.bump();
        }
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
                // Permit `else` on the line after `}` (newlines are separators).
                self.skip_newlines();
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
            Tok::Match => {
                self.bump();
                self.parse_match(sp)
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

    /// Parse the rest of a `match` after the `match` keyword (whose span is
    /// `start`): the scrutinee, then `{ pattern => body, ... }`.
    fn parse_match(&mut self, start: Span) -> Expr {
        // Suppress record-literal reading of the scrutinee so `match x {` opens
        // the arm block rather than a record (same trick `if`/`ensure` use).
        let saved = self.no_record_lit;
        self.no_record_lit = true;
        let scrutinee = self.parse_expr();
        self.no_record_lit = false;
        self.expect(Tok::LBrace);
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            let pat = self.parse_pattern();
            self.expect(Tok::FatArrow);
            let body = self.parse_expr();
            let aspan = pat.span().to(body.span());
            arms.push(MatchArm {
                pattern: pat,
                body,
                span: aspan,
            });
            self.skip_newlines();
            self.eat(&Tok::Comma);
        }
        let rb = self.expect(Tok::RBrace);
        self.no_record_lit = saved;
        Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: start.to(rb),
        }
    }

    fn parse_pattern(&mut self) -> Pattern {
        let sp = self.peek_span();
        match self.peek().clone() {
            Tok::Ident(name) if name == "_" => {
                self.bump();
                Pattern::Wildcard { span: sp }
            }
            Tok::Ident(name) => {
                self.bump();
                Pattern::Variant { name, span: sp }
            }
            _ => {
                self.error_here("expected a pattern (a variant name or `_`)");
                self.bump();
                Pattern::Wildcard { span: sp }
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

    const SUM: &str = r#"
type Parity = Even | Odd

fn describe(p: Parity) -> String {
  return match p {
    Even => "even"
    Odd => "odd"
  }
}
"#;

    #[test]
    fn parses_sum_type_and_match() {
        let (module, diags) = parse("sum.tach", SUM);
        let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "unexpected parse errors: {:?}", errs);

        // the sum type declares two payload-less variants
        let parity = module.items.iter().find_map(|i| match i {
            Item::Type(t) if t.name == "Parity" => Some(t),
            _ => None,
        });
        match parity.map(|t| &t.ty) {
            Some(TypeExpr::Sum { variants, .. }) => {
                let names: Vec<_> = variants.iter().map(|v| v.name.as_str()).collect();
                assert_eq!(names, vec!["Even", "Odd"]);
            }
            other => panic!("expected a sum type, got {:?}", other),
        }

        // the body is a single `return match ...` with two arms
        let f = module.items.iter().find_map(|i| match i {
            Item::Fn(f) if f.name == "describe" => Some(f),
            _ => None,
        });
        let f = f.expect("describe fn parsed");
        match &f.body.stmts[0] {
            Stmt::Return {
                value: Some(Expr::Match { arms, .. }),
                ..
            } => {
                assert_eq!(arms.len(), 2);
                assert!(matches!(arms[0].pattern, Pattern::Variant { .. }));
            }
            other => panic!("expected `return match`, got {:?}", other),
        }
    }

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

    #[test]
    fn parses_a_goal_with_every_section() {
        let src = r#"goal FixFailingTests -> Success {
  budget {
    steps: 30
    retries: 3
    time: 20m
    cost: 0
  }
  allow {
    effect db.read
    effect log.write
    fs.read "."
    fs.write ["src/**", "tests/**"]
    shell.run ["cargo test", "bun test"]
    tach.check
  }
  require {
    tests.pass
    no_new_effects
  }
}
"#;
        let (module, diags) = parse("goal.tach", src);
        let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "unexpected parse errors: {:?}", errs);
        let g = module
            .items
            .iter()
            .find_map(|i| match i {
                Item::Goal(g) => Some(g),
                _ => None,
            })
            .expect("goal parsed");
        assert_eq!(g.name, "FixFailingTests");
        assert_eq!(g.success.as_deref(), Some("Success"));
        assert_eq!(g.budget.steps, Some(30));
        assert_eq!(g.budget.retries, Some(3));
        assert_eq!(g.budget.time.as_deref(), Some("20m"));
        assert_eq!(g.budget.cost, Some(0));
        assert_eq!(g.allow.effects.len(), 2);
        assert_eq!(g.allow.effects[0].name, "db.read");
        assert_eq!(g.allow.fs_read, vec!["."]);
        assert_eq!(g.allow.fs_write, vec!["src/**", "tests/**"]);
        assert_eq!(g.allow.shell, vec!["cargo test", "bun test"]);
        assert_eq!(g.allow.tools, vec!["tach.check"]);
        assert_eq!(g.require.conditions.len(), 2);
        assert_eq!(g.require.conditions[0].name, "tests.pass");
        assert!(g.plan.is_none(), "a repair goal has no plan block");
    }

    #[test]
    fn parses_a_goal_with_a_plan_block() {
        let src = r#"goal ReconcileCharges -> Success {
  budget {
    steps: 60
  }
  allow {
    fake.stripe.list_disputes
    fake.stripe.refund
    fake.email.send
    fake.zendesk.comment
  }
  require {
    refunds.receipted
  }
  plan {
    let disputes = call fake.stripe.list_disputes { customer: "cus_42" }
    for charge in disputes.charges {
      if charge.is_duplicate {
        approve "refund the duplicate" {
          let refund = call fake.stripe.refund { charge_id: charge.charge_id, amount_cents: charge.amount_cents, reason: "duplicate" }
          call fake.email.send { to: "billing@acme.test", template: "refund_issued", charge_id: charge.charge_id }
        }
      } else {
        call fake.zendesk.comment { ticket_id: "zd_dispute", body: "Not a duplicate.", public: false }
      }
    }
  }
}
"#;
        let (module, diags) = parse("plan.tach", src);
        let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "unexpected parse errors: {:?}", errs);
        let g = module
            .items
            .iter()
            .find_map(|i| match i {
                Item::Goal(g) => Some(g),
                _ => None,
            })
            .expect("goal parsed");
        let plan = g.plan.as_ref().expect("plan block parsed");
        assert_eq!(plan.stmts.len(), 2, "let + for at the top level");
        // stmt 0: let disputes = call ...
        match &plan.stmts[0] {
            PlanStmt::Let {
                name,
                value: PlanValue::Call(c),
                ..
            } => {
                assert_eq!(name, "disputes");
                assert_eq!(c.tool, "fake.stripe.list_disputes");
                assert_eq!(c.input.len(), 1);
                assert_eq!(c.input[0].0, "customer");
            }
            other => panic!("expected `let .. = call`, got {:?}", other),
        }
        // stmt 1: for charge in disputes.charges { if .. else .. }
        match &plan.stmts[1] {
            PlanStmt::For {
                var, iter, body, ..
            } => {
                assert_eq!(var, "charge");
                assert!(matches!(iter, Expr::Field { name, .. } if name == "charges"));
                assert_eq!(body.len(), 1, "the loop body is a single if/else");
                match &body[0] {
                    PlanStmt::If {
                        cond,
                        then,
                        els: Some(els),
                        ..
                    } => {
                        assert!(matches!(cond, Expr::Field { name, .. } if name == "is_duplicate"));
                        // then-branch is a single approve gate with a 2-call body
                        match &then[0] {
                            PlanStmt::Approve { summary, body, .. } => {
                                assert_eq!(summary, "refund the duplicate");
                                assert_eq!(body.len(), 2, "refund + email inside the gate");
                                assert!(matches!(&body[0], PlanStmt::Let { .. }));
                                assert!(matches!(&body[1], PlanStmt::Call { .. }));
                            }
                            other => panic!("expected approve gate, got {:?}", other),
                        }
                        // else-branch is a single bare call
                        assert!(matches!(&els[0], PlanStmt::Call { .. }));
                    }
                    other => panic!("expected if/else, got {:?}", other),
                }
            }
            other => panic!("expected a for loop, got {:?}", other),
        }
    }

    #[test]
    fn plan_tolerates_newline_before_else_and_call_brace() {
        // `else` on its own line, and a call whose input record opens on the next
        // line — both must parse (newlines are otherwise statement separators).
        let src = r#"goal Forgiving -> Success {
  allow {
    fake.email.send
  }
  plan {
    if true {
      call fake.email.send
        { to: "a@b.c", template: "x" }
    }
    else {
      call fake.email.send { to: "d@e.f" }
    }
  }
}
"#;
        let (module, diags) = parse("forgiving.tach", src);
        let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "unexpected parse errors: {:?}", errs);
        let g = module
            .items
            .iter()
            .find_map(|i| match i {
                Item::Goal(g) => Some(g),
                _ => None,
            })
            .expect("goal parsed");
        let plan = g.plan.as_ref().expect("plan block");
        match &plan.stmts[0] {
            PlanStmt::If {
                then,
                els: Some(els),
                ..
            } => {
                assert!(matches!(&then[0], PlanStmt::Call { .. }), "multi-line call");
                assert!(matches!(&els[0], PlanStmt::Call { .. }), "else attached");
            }
            other => panic!("expected if/else with attached else, got {:?}", other),
        }
    }
}
