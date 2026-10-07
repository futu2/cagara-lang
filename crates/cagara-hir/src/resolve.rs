//! Name resolution as salsa queries.

use crate::db::ModuleInput;
use crate::lower::parse_module;
use crate::primitive::PRIMS;
use crate::workspace::Binding;
use std::collections::HashMap;
use std::sync::Arc;

/// A module's own definitions (what `import` and `alias.name` see). A name
/// defined more than once is an overload set.
#[salsa::tracked(returns(ref))]
pub fn module_own(db: &dyn salsa::Database, input: ModuleInput) -> Arc<HashMap<String, Binding>> {
    let m = *input.index(db);
    let defs = &parse_module(db, *input.file(db)).module.defs;
    let mut groups = HashMap::<String, Vec<usize>>::new();
    for (i, d) in defs.iter().enumerate() {
        groups.entry(d.name.clone()).or_default().push(i);
    }
    Arc::new(
        groups
            .into_iter()
            .map(|(n, is)| {
                let b = if is.len() == 1 {
                    Binding::Def(m, is[0])
                } else {
                    Binding::Overloads(m, is)
                };
                (n, b)
            })
            .collect(),
    )
}

/// Names visible in a module: the prelude (or, in the prelude, the `__`
/// primitives), then its imports, then its own definitions (which shadow).
#[salsa::tracked(returns(ref))]
pub fn module_scope(db: &dyn salsa::Database, input: ModuleInput) -> Arc<HashMap<String, Binding>> {
    let mut scope = HashMap::new();
    match input.prelude(db) {
        None => {
            for (n, p) in PRIMS {
                scope.insert(n.to_string(), Binding::Prim(*p));
            }
        }
        Some(p) => scope.extend(
            module_own(db, *p)
                .iter()
                .map(|(n, b)| (n.clone(), b.clone())),
        ),
    }
    for (alias, t) in input.imports(db) {
        match alias {
            Some(a) => {
                scope.insert(a.clone(), Binding::Module(*t.index(db)));
            }
            None => scope.extend(
                module_own(db, *t)
                    .iter()
                    .map(|(n, b)| (n.clone(), b.clone())),
            ),
        }
    }
    scope.extend(
        module_own(db, input)
            .iter()
            .map(|(n, b)| (n.clone(), b.clone())),
    );
    Arc::new(scope)
}
