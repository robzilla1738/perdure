use crate::types::Type;

/// The set of builtin module names. A `Method` call whose receiver is one of
/// these idents (and isn't shadowed by a local) is a builtin module call — it
/// requires an `import` and contributes an effect.
pub fn is_module(name: &str) -> bool {
    matches!(name, "db" | "time" | "log" | "net" | "math")
}

/// Signature of a builtin module member: the effect it performs (if any) and
/// its return type.
pub struct BuiltinFn {
    pub effect: Option<&'static str>,
    pub ret: Type,
}

/// Resolve `module.member` to its signature, or `None` if it doesn't exist.
pub fn module_member(module: &str, member: &str) -> Option<BuiltinFn> {
    let (effect, ret): (Option<&'static str>, Type) = match (module, member) {
        ("db", "query") => (
            Some("db.read"),
            Type::Result(Box::new(Type::Unknown), Box::new(Type::Unknown)),
        ),
        ("db", "seed") => (Some("db.write"), Type::Unit),
        ("db", "exec") => (Some("db.write"), Type::Unit),
        ("time", "now") => (Some("time.read"), Type::Int),
        ("log", "info") => (Some("log.write"), Type::Unit),
        ("log", "warn") => (Some("log.write"), Type::Unit),
        ("net", "get") => (
            Some("net.read"),
            Type::Result(Box::new(Type::Str), Box::new(Type::Unknown)),
        ),
        ("net", "post") => (
            Some("net.write"),
            Type::Result(Box::new(Type::Str), Box::new(Type::Unknown)),
        ),
        ("math", "abs") => (None, Type::Int),
        ("math", "max") => (None, Type::Int),
        ("math", "min") => (None, Type::Int),
        _ => return None,
    };
    Some(BuiltinFn { effect, ret })
}

/// All effect labels Tach understands, in a stable order. Used by `tach audit`
/// and to validate that a declared effect actually names a real effect.
pub const KNOWN_EFFECTS: &[&str] = &[
    "db.read",
    "db.write",
    "time.read",
    "log.write",
    "net.read",
    "net.write",
];

/// A short, human description of what an effect lets code do — surfaced by
/// `tach audit` so a reviewer (or agent) instantly knows the blast radius.
pub fn effect_description(effect: &str) -> &'static str {
    match effect {
        "db.read" => "reads from the database",
        "db.write" => "writes to the database",
        "time.read" => "reads the wall clock",
        "log.write" => "writes to the log",
        "net.read" => "makes outbound network reads",
        "net.write" => "makes outbound network writes (can mutate the world)",
        _ => "unknown effect",
    }
}

/// Effects considered "dangerous" — money movement, network egress, writes.
/// `tach audit` flags these so they get extra scrutiny.
pub fn is_sensitive(effect: &str) -> bool {
    matches!(effect, "net.write" | "net.read" | "db.write")
}
