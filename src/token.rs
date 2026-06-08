use crate::span::Span;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    // literals
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),

    // keywords
    Fn,
    Let,
    Type,
    Return,
    Effects,
    Import,
    Test,
    Ensure,
    If,
    Else,
    True,
    False,
    Ok,
    Err,

    // punctuation & operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    Arrow,
    Question,
    Eq,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    AndAnd,
    OrOr,
    Bang,

    /// Statement separator (collapsed runs of newlines, suppressed inside `()`/`[]`).
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub span: Span,
}

impl Tok {
    /// Map an identifier string to its keyword token, if any.
    pub fn keyword(s: &str) -> Option<Tok> {
        Some(match s {
            "fn" => Tok::Fn,
            "let" => Tok::Let,
            "type" => Tok::Type,
            "return" => Tok::Return,
            "effects" => Tok::Effects,
            "import" => Tok::Import,
            "test" => Tok::Test,
            "ensure" => Tok::Ensure,
            "if" => Tok::If,
            "else" => Tok::Else,
            "true" => Tok::True,
            "false" => Tok::False,
            "Ok" => Tok::Ok,
            "Err" => Tok::Err,
            _ => return None,
        })
    }

    /// Human-readable name for error messages.
    pub fn human(&self) -> String {
        match self {
            Tok::Int(_) => "integer".into(),
            Tok::Float(_) => "float".into(),
            Tok::Str(_) => "string".into(),
            Tok::Ident(s) => format!("identifier `{}`", s),
            Tok::Fn => "`fn`".into(),
            Tok::Let => "`let`".into(),
            Tok::Type => "`type`".into(),
            Tok::Return => "`return`".into(),
            Tok::Effects => "`effects`".into(),
            Tok::Import => "`import`".into(),
            Tok::Test => "`test`".into(),
            Tok::Ensure => "`ensure`".into(),
            Tok::If => "`if`".into(),
            Tok::Else => "`else`".into(),
            Tok::True => "`true`".into(),
            Tok::False => "`false`".into(),
            Tok::Ok => "`Ok`".into(),
            Tok::Err => "`Err`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Colon => "`:`".into(),
            Tok::Dot => "`.`".into(),
            Tok::Arrow => "`->`".into(),
            Tok::Question => "`?`".into(),
            Tok::Eq => "`=`".into(),
            Tok::EqEq => "`==`".into(),
            Tok::NotEq => "`!=`".into(),
            Tok::Lt => "`<`".into(),
            Tok::LtEq => "`<=`".into(),
            Tok::Gt => "`>`".into(),
            Tok::GtEq => "`>=`".into(),
            Tok::Plus => "`+`".into(),
            Tok::Minus => "`-`".into(),
            Tok::Star => "`*`".into(),
            Tok::Slash => "`/`".into(),
            Tok::AndAnd => "`&&`".into(),
            Tok::OrOr => "`||`".into(),
            Tok::Bang => "`!`".into(),
            Tok::Newline => "newline".into(),
            Tok::Eof => "end of file".into(),
        }
    }
}
