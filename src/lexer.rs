use crate::diagnostics::Diagnostic;
use crate::span::Span;
use crate::token::{Tok, Token};

/// Tokenize Tach source.
///
/// Newlines are significant statement separators, but are suppressed while
/// nested inside `(` or `[` so multi-line call arguments and `effects [...]`
/// clauses read naturally. Runs of blank lines collapse to a single `Newline`.
pub fn lex(file: &str, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let n = chars.len();
    let src_len = src.len();
    let end_off = |idx: usize| -> usize {
        if idx < n {
            chars[idx].0
        } else {
            src_len
        }
    };

    let mut toks: Vec<Token> = Vec::new();
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut i = 0usize;
    let mut depth: i32 = 0; // () and [] nesting

    while i < n {
        let (off, c) = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                i += 1;
            }
            '\n' => {
                i += 1;
                if depth <= 0 {
                    // collapse consecutive newlines into one separator
                    if !matches!(toks.last().map(|t| &t.kind), Some(Tok::Newline)) {
                        toks.push(Token {
                            kind: Tok::Newline,
                            span: Span::new(off, off + 1),
                        });
                    }
                }
            }
            '/' if i + 1 < n && chars[i + 1].1 == '/' => {
                // line comment
                while i < n && chars[i].1 != '\n' {
                    i += 1;
                }
            }
            '"' => {
                let start = off;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < n {
                    let (_, ch) = chars[i];
                    if ch == '\\' && i + 1 < n {
                        let (_, esc) = chars[i + 1];
                        match esc {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            '"' => s.push('"'),
                            '\\' => s.push('\\'),
                            other => {
                                s.push('\\');
                                s.push(other);
                            }
                        }
                        i += 2;
                        continue;
                    }
                    if ch == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                let span = Span::new(start, end_off(i));
                if !closed {
                    diags.push(Diagnostic::error(
                        "E0001",
                        "syntax",
                        "unterminated string literal",
                        file,
                        span,
                    ));
                }
                toks.push(Token {
                    kind: Tok::Str(s),
                    span,
                });
            }
            c if c.is_ascii_digit() => {
                let start = off;
                let mut j = i;
                while j < n && chars[j].1.is_ascii_digit() {
                    j += 1;
                }
                let mut is_float = false;
                if j < n && chars[j].1 == '.' && j + 1 < n && chars[j + 1].1.is_ascii_digit() {
                    is_float = true;
                    j += 1;
                    while j < n && chars[j].1.is_ascii_digit() {
                        j += 1;
                    }
                }
                let text: String = chars[i..j].iter().map(|(_, ch)| *ch).collect();
                let span = Span::new(start, end_off(j));
                let kind = if is_float {
                    Tok::Float(text.parse().unwrap_or(0.0))
                } else {
                    Tok::Int(text.parse().unwrap_or(0))
                };
                toks.push(Token { kind, span });
                i = j;
            }
            c if is_ident_start(c) => {
                let start = off;
                let mut j = i;
                while j < n && is_ident_cont(chars[j].1) {
                    j += 1;
                }
                let text: String = chars[i..j].iter().map(|(_, ch)| *ch).collect();
                let span = Span::new(start, end_off(j));
                let kind = Tok::keyword(&text).unwrap_or(Tok::Ident(text));
                toks.push(Token { kind, span });
                i = j;
            }
            _ => {
                // operators & punctuation
                let two: Option<(Tok, usize)> = if i + 1 < n {
                    let c2 = chars[i + 1].1;
                    match (c, c2) {
                        ('-', '>') => Some((Tok::Arrow, 2)),
                        ('=', '=') => Some((Tok::EqEq, 2)),
                        ('!', '=') => Some((Tok::NotEq, 2)),
                        ('<', '=') => Some((Tok::LtEq, 2)),
                        ('>', '=') => Some((Tok::GtEq, 2)),
                        ('&', '&') => Some((Tok::AndAnd, 2)),
                        ('|', '|') => Some((Tok::OrOr, 2)),
                        _ => None,
                    }
                } else {
                    None
                };

                let (kind, width) = if let Some((k, w)) = two {
                    (k, w)
                } else {
                    let single = match c {
                        '(' => {
                            depth += 1;
                            Tok::LParen
                        }
                        ')' => {
                            depth -= 1;
                            Tok::RParen
                        }
                        '[' => {
                            depth += 1;
                            Tok::LBracket
                        }
                        ']' => {
                            depth -= 1;
                            Tok::RBracket
                        }
                        '{' => Tok::LBrace,
                        '}' => Tok::RBrace,
                        ',' => Tok::Comma,
                        ':' => Tok::Colon,
                        '.' => Tok::Dot,
                        '?' => Tok::Question,
                        '=' => Tok::Eq,
                        '<' => Tok::Lt,
                        '>' => Tok::Gt,
                        '+' => Tok::Plus,
                        '-' => Tok::Minus,
                        '*' => Tok::Star,
                        '/' => Tok::Slash,
                        '!' => Tok::Bang,
                        other => {
                            let span = Span::new(off, end_off(i + 1));
                            diags.push(Diagnostic::error(
                                "E0002",
                                "syntax",
                                format!("unexpected character `{}`", other),
                                file,
                                span,
                            ));
                            i += 1;
                            continue;
                        }
                    };
                    (single, 1)
                };
                let span = Span::new(off, end_off(i + width));
                toks.push(Token { kind, span });
                i += width;
            }
        }
    }

    // trailing newline so the last statement is cleanly terminated
    if !matches!(toks.last().map(|t| &t.kind), Some(Tok::Newline)) {
        toks.push(Token {
            kind: Tok::Newline,
            span: Span::new(src_len, src_len),
        });
    }
    toks.push(Token {
        kind: Tok::Eof,
        span: Span::new(src_len, src_len),
    });
    (toks, diags)
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_ident_cont(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}
