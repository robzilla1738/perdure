use crate::span::Span;
use serde::{Deserialize, Serialize};

/// Severity of a diagnostic. Errors block compilation; warnings are lints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// A concrete, machine-applicable edit the compiler believes will repair a
/// diagnostic. This is the heart of Tach's agent-native story: the compiler does
/// not merely point at a problem, it proposes a span-replacement an agent can
/// apply verbatim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreferredPatch {
    pub file: String,
    pub span: Span,
    pub replacement: String,
    pub rationale: String,
}

/// A single structured compiler diagnostic.
///
/// Designed to be rendered two ways from one source of truth: a friendly,
/// caret-underlined block for humans, and a stable JSON object for agents. The
/// agent-facing fields (`kind`, `repair_strategies`, `preferred_patch`) are
/// first-class, not an afterthought.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable error code, e.g. `E0421`.
    pub code: String,
    pub severity: Severity,
    /// Machine-stable category an agent can switch on, e.g. `effect_undeclared`.
    pub kind: String,
    /// One-line human summary.
    pub message: String,
    pub file: String,
    pub span: Span,
    /// Ordered list of repair strategies, most-preferred first.
    #[serde(default)]
    pub repair_strategies: Vec<String>,
    /// The single edit the compiler would apply if asked to auto-fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_patch: Option<PreferredPatch>,
    /// Extra human context (not required for repair).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(
        code: &str,
        kind: &str,
        message: impl Into<String>,
        file: &str,
        span: Span,
    ) -> Self {
        Diagnostic {
            code: code.to_string(),
            severity: Severity::Error,
            kind: kind.to_string(),
            message: message.into(),
            file: file.to_string(),
            span,
            repair_strategies: Vec::new(),
            preferred_patch: None,
            notes: Vec::new(),
        }
    }

    pub fn warning(
        code: &str,
        kind: &str,
        message: impl Into<String>,
        file: &str,
        span: Span,
    ) -> Self {
        let mut d = Diagnostic::error(code, kind, message, file, span);
        d.severity = Severity::Warning;
        d
    }

    pub fn with_strategies(mut self, strategies: &[&str]) -> Self {
        self.repair_strategies = strategies.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_patch(mut self, patch: PreferredPatch) -> Self {
        self.preferred_patch = Some(patch);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}
