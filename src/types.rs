use crate::ast::{TypeDecl, TypeExpr};
use std::collections::HashMap;

/// A resolved Tach type.
///
/// `Unknown` is the inference/error-recovery hole and is compatible with
/// everything — the checker stays lenient so it only ever reports a type error
/// it is genuinely sure about. False positives are worse than misses here,
/// because an agent will dutifully "fix" a non-problem.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    /// An opaque or user-declared named type (`UserId`, `Session`, `AuthError`).
    Named(String),
    Record(Vec<(String, Type)>),
    Result(Box<Type>, Box<Type>),
    Unknown,
}

impl Type {
    pub fn display(&self) -> String {
        match self {
            Type::Int => "Int".into(),
            Type::Float => "Float".into(),
            Type::Bool => "Bool".into(),
            Type::Str => "String".into(),
            Type::Unit => "Unit".into(),
            Type::Named(n) => n.clone(),
            Type::Record(fields) => {
                let inner: Vec<String> = fields
                    .iter()
                    .map(|(n, t)| format!("{}: {}", n, t.display()))
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
            Type::Result(a, b) => format!("Result<{}, {}>", a.display(), b.display()),
            Type::Unknown => "?".into(),
        }
    }
}

/// Convert a syntactic type into a resolved `Type`. Builtin scalar names are
/// recognized; everything else becomes `Named`.
pub fn type_from_ast(t: &TypeExpr) -> Type {
    match t {
        TypeExpr::Record { fields, .. } => Type::Record(
            fields
                .iter()
                .map(|(n, ft)| (n.clone(), type_from_ast(ft)))
                .collect(),
        ),
        TypeExpr::Name { name, args, .. } => match name.as_str() {
            "Int" => Type::Int,
            "Float" => Type::Float,
            "Bool" => Type::Bool,
            "String" => Type::Str,
            "Unit" => Type::Unit,
            "Result" => {
                let a = args.first().map(type_from_ast).unwrap_or(Type::Unknown);
                let b = args.get(1).map(type_from_ast).unwrap_or(Type::Unknown);
                Type::Result(Box::new(a), Box::new(b))
            }
            other => Type::Named(other.to_string()),
        },
    }
}

/// Lookup table for user-declared record types so the checker can resolve a
/// `Named` type to its fields.
#[derive(Clone, Debug, Default)]
pub struct TypeRegistry {
    records: HashMap<String, Vec<(String, Type)>>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        TypeRegistry::default()
    }

    pub fn add_decl(&mut self, d: &TypeDecl) {
        if let Type::Record(fields) = type_from_ast(&d.ty) {
            self.records.insert(d.name.clone(), fields);
        }
    }

    pub fn record_fields(&self, name: &str) -> Option<&Vec<(String, Type)>> {
        self.records.get(name)
    }

    pub fn is_known(&self, name: &str) -> bool {
        self.records.contains_key(name)
    }
}

/// Structural compatibility check, lenient on `Unknown` and resolving `Named`
/// record types through the registry.
pub fn compatible(a: &Type, b: &Type, reg: &TypeRegistry) -> bool {
    use Type::*;
    match (a, b) {
        (Unknown, _) | (_, Unknown) => true,
        (Int, Int) | (Float, Float) | (Bool, Bool) | (Str, Str) | (Unit, Unit) => true,
        (Named(x), Named(y)) => x == y,
        (Named(x), other) | (other, Named(x)) => match reg.record_fields(x) {
            Some(fields) => compatible(&Record(fields.clone()), other, reg),
            None => false,
        },
        (Result(a1, a2), Result(b1, b2)) => compatible(a1, b1, reg) && compatible(a2, b2, reg),
        (Record(fa), Record(fb)) => {
            fa.len() == fb.len()
                && fa
                    .iter()
                    .all(|(n, t)| fb.iter().any(|(n2, t2)| n == n2 && compatible(t, t2, reg)))
        }
        _ => false,
    }
}
