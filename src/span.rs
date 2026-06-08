use serde::{Deserialize, Serialize};

/// A half-open byte range `[start, end)` into a source file.
///
/// Spans are byte offsets so they double as edit coordinates: a patch is just a
/// span plus replacement text. Keeping a single coordinate space for "where the
/// error is" and "where to edit" is what makes machine repair clean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// Zero-width span at `offset` — an insertion point.
    pub fn at(offset: usize) -> Self {
        Span {
            start: offset,
            end: offset,
        }
    }

    pub fn dummy() -> Self {
        Span { start: 0, end: 0 }
    }

    /// Smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}
