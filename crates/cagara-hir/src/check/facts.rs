//! Exact inputs to elaborating one definition, memoized by Salsa.
//!
//! Source and locations are retained alongside closed checker facts. Equality
//! is the reuse proof; a source hash is only an identity hint. Dependencies
//! include every overload candidate and conservatively include names shadowed
//! by lambda parameters, since several elaboration paths inspect the scope.

use super::*;
use crate::compile::DefinitionId;

#[derive(Debug)]
pub(crate) struct DefinitionFacts {
    source: String,
    failed: bool,
    holes: usize,
    scheme: Option<SchemeView>,
    result: Option<(Phase, ScalarType)>,
    choices: Vec<(u32, usize, Choice)>,
    uses: Vec<(u32, Option<ScalarType>)>,
    bindings: Vec<Option<Binding>>,
    pub dependencies: Vec<DefinitionId>,
}

/// Equal when the definition is the same definition.
///
/// Written out rather than derived because it deliberately leaves out *where*
/// the definition sits. Its own `source` already says what it is; an offset
/// says only where it is, and a definition that moved is not a definition that
/// changed. Deriving the comparison made every definition below a keystroke
/// look edited — their spans had all shifted — so a character typed near the
/// top of a file re-elaborated the whole file. The cached result of a
/// definition that only moved is reused, and its locations are moved onto the
/// current text by the shell that owns the cache (`crate::incremental`).
impl PartialEq for DefinitionFacts {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && self.failed == other.failed
            && self.holes == other.holes
            && self.scheme == other.scheme
            && self.result == other.result
            && self.choices == other.choices
            && self.uses == other.uses
            && self.bindings == other.bindings
            && self.dependencies == other.dependencies
    }
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
    // Both facts come off the definition's typed tree: the choices recorded at
    // each node's site, and the count of overloads it leaves open.
    let typed = checked.typed.get(&(module, def));
    let mut choices: Vec<(u32, usize, Choice)> = typed
        .into_iter()
        .flat_map(|body| body.nodes())
        .flat_map(|node| {
            node.choices
                .iter()
                .map(move |(hole, choice)| (node.id, *hole, *choice))
        })
        .collect();
    choices.sort_by_key(|&(site, hole, _)| (site, hole));
    let mut facts = DefinitionFacts {
        source: file.text(db)[definition.span.start as usize..definition.span.end as usize].into(),
        failed: scheme.is_none_or(|s| s.failed),
        holes: typed.map_or(0, |body| body.holes),
        scheme: scheme.map(|s| scheme_view(&s.ty)),
        result: scheme.and_then(|s| scheme_result_expr(&s.ty)),
        choices,
        uses: Vec::new(),
        bindings: Vec::new(),
        dependencies: Vec::new(),
    };
    let mut pending = vec![&definition.body];
    while let Some(e) = pending.pop() {
        // The node's own classification, so the reuse proof and elaboration
        // read one account of what an expression got.
        let ty = typed
            .and_then(|body| body.node(e.id))
            .and_then(|node| node.ty());
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
            }
            _ => {}
        }
        // Pushed in reverse so the stack pops them in source order, which is
        // the order `child_exprs` states once.
        pending.extend(child_exprs(e).into_iter().rev());
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
            Some(Binding::Module(_)) | None => {}
        }
    }
}

/// The sub-expressions of one expression, in source order.
///
/// One rule, in one place, and an exhaustive match: a new `ExprKind` is a
/// compile error here rather than a child silently left unvisited. The facts
/// walk above reads it, and a typed tree built from source must visit the same
/// children in the same order or the two disagree about which node a fact
/// belongs to.
pub(crate) fn child_exprs(e: &cagara_syntax::ast::Expr) -> Vec<&cagara_syntax::ast::Expr> {
    use cagara_syntax::ast::ExprKind;
    match &e.kind {
        ExprKind::App(function, args) => {
            let mut out = Vec::with_capacity(args.len() + 1);
            out.push(function.as_ref());
            out.extend(args.iter());
            out
        }
        ExprKind::Lambda(_, body) => vec![body],
        ExprKind::Record(fields) => fields.iter().map(|(_, value)| value).collect(),
        ExprKind::List(values) => values.iter().collect(),
        ExprKind::Case(arms) => arms
            .iter()
            .flat_map(|(cond, value)| [cond, value])
            .collect(),
        ExprKind::Proj(base, _) => vec![base],
        ExprKind::Name(_)
        | ExprKind::Lit(_)
        | ExprKind::Field(_, _)
        | ExprKind::Sql(_)
        | ExprKind::Primitive(_)
        | ExprKind::Error => Vec::new(),
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
    fn child_exprs_visits_every_sub_expression_in_source_order() {
        use cagara_syntax::ast::ExprKind;
        // An application: the callee, then the arguments as written.
        let ws = Workspace::from_source("q = f 1 2\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        let ExprKind::App(function, args) = &body.kind else {
            panic!("the body should be an application");
        };
        let children = child_exprs(body);
        assert_eq!(children.len(), 3, "callee and two arguments");
        assert_eq!(children[0].id, function.id);
        assert_eq!(children[1].id, args[0].id);
        assert_eq!(children[2].id, args[1].id);

        // A `case`: each arm's condition before its value, arms in order.
        // Arms are `;`-separated (see `cagara-syntax`'s `case_arms_are_semicolon_separated`).
        let ws = Workspace::from_source("q = case { a => 1; b => 2 }\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        let ExprKind::Case(arms) = &body.kind else {
            panic!("the body should be a case");
        };
        let children = child_exprs(body);
        assert_eq!(children.len(), 4, "two arms, a condition and a value each");
        assert_eq!(children[0].id, arms[0].0.id);
        assert_eq!(children[1].id, arms[0].1.id);
        assert_eq!(children[2].id, arms[1].0.id);
        assert_eq!(children[3].id, arms[1].1.id);

        // A projection's base is its only child; a leaf has none. `.a` is a
        // `Field` — a leaf — while `base.a` is the `Proj` that has a base.
        let ws = Workspace::from_source("q = users.id\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        assert_eq!(child_exprs(body).len(), 1, "a projection's base");
        let leaf = child_exprs(body)[0];
        assert!(child_exprs(leaf).is_empty(), "a name has no children");

        let ws = Workspace::from_source("q = .a\n");
        let body = &ws.modules[ws.root].module.defs[0].body;
        assert!(child_exprs(body).is_empty(), "a field is a leaf");
    }

    #[test]
    fn facts_are_memoized_and_compare_checked_types_but_not_locations() {
        let src = "q : query { a = int } = table \"s\" \"t\"\n";
        let mut ws = Workspace::from_source(src);
        let input = ws.inputs[ws.root];
        let first = Arc::clone(definition_facts(&ws.db, input, 0));
        let runs = FACT_RUNS.with(|runs| runs.get());
        assert!(Arc::ptr_eq(&first, definition_facts(&ws.db, input, 0)));
        assert_eq!(FACT_RUNS.with(|runs| runs.get()), runs);

        // A definition that only moved is the same definition: its source text
        // and its checked facts are unchanged, and the offset it now sits at is
        // not part of either. The cached result of one that moved is reused and
        // relocated, so treating the move as a change would rebuild the file
        // for a keystroke typed above it.
        assert!(ws.set_source(ws.root, format!("# moved\n{src}")));
        assert_eq!(first, *definition_facts(&ws.db, input, 0));

        // Its checked type is part of what it is.
        assert!(ws.set_source(ws.root, src.replace("int", "bool")));
        assert_ne!(first, *definition_facts(&ws.db, input, 0));
    }
}
