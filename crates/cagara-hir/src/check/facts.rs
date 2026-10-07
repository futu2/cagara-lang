//! Exact inputs to elaborating one definition, memoized by Salsa.
//!
//! Source and locations are retained alongside closed checker facts. Equality
//! is the reuse proof; a source hash is only an identity hint. Dependencies
//! include every overload candidate and conservatively include names shadowed
//! by lambda parameters, since several elaboration paths inspect the scope.

use super::*;
use crate::compile::DefinitionId;

#[derive(Debug, PartialEq)]
pub(crate) struct DefinitionFacts {
    source: String,
    span: Span,
    failed: bool,
    holes: usize,
    scheme: Option<SchemeView>,
    result: Option<(Phase, ScalarType)>,
    choices: Vec<(u32, usize, Choice)>,
    uses: Vec<(u32, Option<ScalarType>)>,
    bindings: Vec<Option<Binding>>,
    pub dependencies: Vec<DefinitionId>,
}

#[salsa::tracked(returns(ref))]
pub(crate) fn definition_facts(
    db: &dyn salsa::Database,
    input: ModuleInput,
    def: usize,
) -> Arc<DefinitionFacts> {
    #[cfg(test)]
    FACT_RUNS.with(|runs| runs.set(runs.get() + 1));
    let module = *input.index(db);
    let file = *input.file(db);
    let parsed = parse_module(db, file);
    let definition = &parsed.module.defs[def];
    let checked = module_check(db, input);
    let scheme = checked.schemes.get(&(module, def));
    let scope = module_scope(db, input);
    let mut choices: Vec<_> = checked
        .choices
        .get(&(module, def))
        .into_iter()
        .flat_map(|choices| choices.iter())
        .map(|(&(site, hole), &choice)| (site, hole, choice))
        .collect();
    choices.sort_by_key(|&(site, hole, _)| (site, hole));
    let mut facts = DefinitionFacts {
        source: file.text(db)[definition.span.start as usize..definition.span.end as usize].into(),
        span: definition.span,
        failed: scheme.is_none_or(|s| s.failed),
        holes: checked.holes.get(&(module, def)).copied().unwrap_or(0),
        scheme: scheme.map(|s| scheme_view(&s.ty)),
        result: scheme.and_then(|s| scheme_result_expr(&s.ty)),
        choices,
        uses: Vec::new(),
        bindings: Vec::new(),
        dependencies: Vec::new(),
    };
    let mut pending = vec![&definition.body];
    while let Some(e) = pending.pop() {
        let ty = checked
            .use_tys
            .get(&(module, e.id))
            .and_then(|t| match scheme_view(t) {
                SchemeView::Scalar(ty) => Some(ty),
                SchemeView::Open => Some(ScalarType::Unknown),
                _ => None,
            });
        facts.uses.push((e.id, ty));
        match &e.kind {
            ExprKind::Name(name) => facts.binding(scope.get(name)),
            ExprKind::Proj(base, field) => {
                if let ExprKind::Name(alias) = &base.kind {
                    if let Some(Binding::Module(target)) = scope.get(alias) {
                        let imported = input
                            .imports(db)
                            .iter()
                            .map(|(_, input)| *input)
                            .find(|input| input.index(db) == target)
                            .expect("module aliases refer to direct imports");
                        facts.binding(module_own(db, imported).get(field));
                    }
                }
                pending.push(base);
            }
            ExprKind::App(function, args) => {
                pending.extend(args.iter().rev());
                pending.push(function);
            }
            ExprKind::Lambda(_, body) => pending.push(body),
            ExprKind::Record(fields) => pending.extend(fields.iter().rev().map(|(_, value)| value)),
            ExprKind::List(values) => pending.extend(values.iter().rev()),
            ExprKind::Lit(_)
            | ExprKind::Field(_, _)
            | ExprKind::Sql(_)
            | ExprKind::Primitive(_)
            | ExprKind::Error => {}
        }
    }
    facts
        .dependencies
        .sort_unstable_by_key(|id| (id.module, id.def));
    facts.dependencies.dedup();
    Arc::new(facts)
}

impl DefinitionFacts {
    fn binding(&mut self, binding: Option<&Binding>) {
        self.bindings.push(binding.cloned());
        match binding {
            Some(Binding::Def(module, def)) => self.dependencies.push(DefinitionId {
                module: *module,
                def: *def,
            }),
            Some(Binding::Overloads(module, defs)) => {
                self.dependencies
                    .extend(defs.iter().map(|def| DefinitionId {
                        module: *module,
                        def: *def,
                    }))
            }
            Some(Binding::Prim(_) | Binding::Module(_)) | None => {}
        }
    }
}

#[cfg(test)]
thread_local! {
    static FACT_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_memoized_and_compare_checked_types_and_locations() {
        let src = "q : query { a = int } = table \"s\" \"t\"\n";
        let mut ws = Workspace::from_source(src);
        let input = ws.inputs[ws.root];
        let first = Arc::clone(definition_facts(&ws.db, input, 0));
        let runs = FACT_RUNS.with(|runs| runs.get());
        assert!(Arc::ptr_eq(&first, definition_facts(&ws.db, input, 0)));
        assert_eq!(FACT_RUNS.with(|runs| runs.get()), runs);
        assert!(ws.set_source(ws.root, format!("# moved\n{src}")));
        assert_ne!(first, *definition_facts(&ws.db, input, 0));
        assert!(ws.set_source(ws.root, src.replace("int", "bool")));
        assert_ne!(first, *definition_facts(&ws.db, input, 0));
    }
}
