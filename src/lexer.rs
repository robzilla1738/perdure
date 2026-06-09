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
                    if ch == '\n' {
                        // A string literal is single-line. A newline before the closing
                        // quote means it is unterminated — stop here rather than swallow
                        // the rest of the file, so the newline still separates statements
                        // and the cascade stays one line instead of erasing the source
                        // below it. `closed` stays false → the E0001 below fires.
                        break;
                    }
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
                // A literal that overflows its type must be an *error*, never a silent
                // `0`: an agent editing Tach would have no idea its `1_0000000000…`
                // became zero, and a miscompiled constant is exactly the kind of quiet
                // wrong-answer this language exists to make impossible.
                let kind = if is_float {
                    match text.parse::<f64>() {
                        Ok(f) if f.is_finite() => Tok::Float(f),
                        _ => {
                            diags.push(Diagnostic::error(
                                "E0002",
                                "number_out_of_range",
                                format!("float literal `{text}` is out of range for Float (f64)"),
                                file,
                                span,
                            ));
                            Tok::Float(0.0)
                        }
                    }
                } else {
                    match text.parse::<i64>() {
                        Ok(n) => Tok::Int(n),
                        Err(_) => {
                            diags.push(Diagnostic::error(
                                "E0002",
                                "number_out_of_range",
                                format!("integer literal `{text}` is out of range for Int (i64)"),
                                file,
                                span,
                            ));
                            Tok::Int(0)
                        }
                    }
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
                        ('=', '>') => Some((Tok::FatArrow, 2)),
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
                        '|' => Tok::Pipe,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_int_literal_is_an_error_not_a_silent_zero() {
        // The headline correctness fix: an out-of-range literal must be flagged, never
        // silently miscompiled to 0 (which an agent editing Tach would never notice).
        let (_t, diags) = lex("t.tach", "fn f() { return 99999999999999999999999999 }");
        assert!(
            diags.iter().any(|d| d.code == "E0002"),
            "overflowing int literal must emit E0002, got {diags:?}"
        );
        // A valid (in-range) i64 still lexes to its exact value, no false positive.
        let (toks, diags) = lex("t.tach", "fn f() { return 9000000000000000000 }");
        assert!(diags.iter().all(|d| d.code != "E0002"));
        assert!(toks
            .iter()
            .any(|t| matches!(t.kind, Tok::Int(9_000_000_000_000_000_000))));
    }

    #[test]
    fn unterminated_string_does_not_swallow_the_rest_of_the_file() {
        // The unclosed string must stop at the newline, so `let y = 5` below still
        // lexes (an Int 5 appears) instead of being eaten as string bytes.
        let src = "fn f() {\n  let s = \"oops\n  let y = 5\n}\n";
        let (toks, diags) = lex("t.tach", src);
        assert!(
            diags.iter().any(|d| d.code == "E0001"),
            "unterminated string must be flagged"
        );
        assert!(
            toks.iter().any(|t| matches!(t.kind, Tok::Int(5))),
            "code after the unterminated string must still lex"
        );
    }
}
