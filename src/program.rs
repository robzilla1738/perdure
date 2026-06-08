use crate::ast::{FnDecl, Item, Module, TestDecl};
use crate::diagnostics::Diagnostic;
use crate::parser::parse;
use crate::source::SourceFile;
use crate::types::TypeRegistry;
use std::collections::{HashMap, HashSet};

/// One parsed source file plus its derived facts.
pub struct Unit {
    pub source: SourceFile,
    pub module: Module,
    pub imports: HashSet<String>,
}

/// A whole Tach program: every source file in a project, parsed.
///
/// The checker walks units one at a time (diagnostics are per-file), while the
/// interpreter sees the merged set of functions and types.
pub struct Program {
    pub units: Vec<Unit>,
}

impl Program {
    pub fn parse_sources(sources: Vec<SourceFile>) -> (Program, Vec<Diagnostic>) {
        let mut units = Vec::new();
        let mut diags = Vec::new();
        for src in sources {
            let (module, ds) = parse(&src.path, &src.text);
            diags.extend(ds);
            let imports = module
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::Import(im) => Some(im.module.clone()),
                    _ => None,
                })
                .collect();
            units.push(Unit {
                source: src,
                module,
                imports,
            });
        }
        (Program { units }, diags)
    }

    /// All functions across the program, keyed by name (last definition wins).
    pub fn functions(&self) -> HashMap<String, &FnDecl> {
        let mut m = HashMap::new();
        for u in &self.units {
            for it in &u.module.items {
                if let Item::Fn(f) = it {
                    m.insert(f.name.clone(), f);
                }
            }
        }
        m
    }

    pub fn type_registry(&self) -> TypeRegistry {
        let mut r = TypeRegistry::new();
        for u in &self.units {
            for it in &u.module.items {
                if let Item::Type(t) = it {
                    r.add_decl(t);
                }
            }
        }
        r
    }

    pub fn tests(&self) -> Vec<&TestDecl> {
        let mut v = Vec::new();
        for u in &self.units {
            for it in &u.module.items {
                if let Item::Test(t) = it {
                    v.push(t);
                }
            }
        }
        v
    }
}
