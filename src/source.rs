use crate::span::Span;

/// A single Tach source file held in memory, with helpers to translate byte
/// offsets into human line/column positions for diagnostics.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub path: String,
    pub text: String,
}

impl SourceFile {
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> Self {
        SourceFile {
            path: path.into(),
            text: text.into(),
        }
    }

    /// 1-based (line, column) for a byte offset.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let mut line = 1usize;
        let mut col = 1usize;
        for (i, c) in self.text.char_indices() {
            if i >= offset {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// The full text of a 1-based line, without the trailing newline.
    pub fn line_text(&self, line: usize) -> &str {
        self.text.lines().nth(line.saturating_sub(1)).unwrap_or("")
    }

    /// The exact source slice a span covers.
    pub fn slice(&self, span: Span) -> &str {
        let end = span.end.min(self.text.len());
        let start = span.start.min(end);
        &self.text[start..end]
    }
}
