use std::collections::BTreeMap;
use std::fmt;

/// A runtime value. Records use a `BTreeMap` so iteration order is deterministic
/// — Tach's whole story rests on reproducible runs, so we never leave ordering
/// to chance.
#[derive(Clone, Debug)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Unit,
    Record(BTreeMap<String, Value>),
    Ok(Box<Value>),
    Err(Box<Value>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Bool(_) => "Bool",
            Value::Str(_) => "String",
            Value::Unit => "Unit",
            Value::Record(_) => "record",
            Value::Ok(_) => "Ok",
            Value::Err(_) => "Err",
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Value::Ok(_))
    }

    pub fn is_err(&self) -> bool {
        matches!(self, Value::Err(_))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A stable string key for use as a map key (the fake DB indexes on this).
    pub fn key(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            other => format!("{}", other),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{}", n),
            Value::Float(x) => write!(f, "{}", x),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Str(s) => write!(f, "\"{}\"", s),
            Value::Unit => write!(f, "()"),
            Value::Record(m) => {
                write!(f, "{{ ")?;
                let mut first = true;
                for (k, v) in m {
                    if !first {
                        write!(f, ", ")?;
                    }
                    first = false;
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, " }}")
            }
            Value::Ok(v) => write!(f, "Ok({})", v),
            Value::Err(v) => write!(f, "Err({})", v),
        }
    }
}
