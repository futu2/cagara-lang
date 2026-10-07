//! The production boundary from checked source to the untyped relational IR.

use crate::check::TypeCheck;
use crate::core::{Error, Fault, Origin};
use crate::elaborate::{elaborate_definition, is_not_a_query};
use crate::ir::Rel;
use crate::workspace::{Binding, Diag, LoadedModule, Workspace};
use cagara_syntax::ast::Module;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Immutable compiler input borrowed from an existing workspace.
///
/// The workspace remains the owner of Salsa storage, overlays, and cache
/// identity. This view only bundles the cached source graph and its type-check
/// result for the source-elaboration and erasure boundary.
#[derive(Clone, Copy)]
pub struct CompilerInput<'a> {
    workspace: &'a Workspace,
    type_check: &'a TypeCheck,
}

impl<'a> CompilerInput<'a> {
    pub fn new(workspace: &'a Workspace, type_check: &'a TypeCheck) -> Self {
        Self {
            workspace,
            type_check,
        }
    }

    pub fn type_check(&self) -> &'a TypeCheck {
        self.type_check
    }

    pub fn diagnostics(&self) -> &'a [Diag] {
        &self.workspace.diags
    }

    pub fn diagnostic(&self, origin: Option<Origin>, message: impl Into<String>) -> Diag {
        match origin {
            Some(origin) => self
                .workspace
                .diag_span(origin.module, origin.span, message),
            None => self.workspace.diag(self.workspace.root, 0, message),
        }
    }

    pub fn root(&self) -> ModuleSnapshot<'a> {
        self.module(self.workspace.root)
    }

    pub fn module(&self, module: usize) -> ModuleSnapshot<'a> {
        ModuleSnapshot {
            workspace: self.workspace,
            module,
        }
    }
}

/// Read-only view of one loaded module in a [`CompilerInput`].
#[derive(Clone, Copy)]
pub struct ModuleSnapshot<'a> {
    workspace: &'a Workspace,
    module: usize,
}

impl<'a> ModuleSnapshot<'a> {
    pub fn index(&self) -> usize {
        self.module
    }

    pub fn path(&self) -> &'a Path {
        &self.loaded().path
    }

    pub fn text(&self) -> &'a str {
        &self.loaded().text
    }

    pub fn source(&self) -> &'a Module {
        &self.loaded().module
    }

    pub fn def(&self, def: usize) -> &'a cagara_syntax::ast::Def {
        &self.source().defs[def]
    }

    pub fn scope(&self) -> &'a HashMap<String, Binding> {
        &self.loaded().scope
    }

    pub fn own(&self) -> &'a HashMap<String, Binding> {
        &self.loaded().own
    }

    fn loaded(&self) -> &'a LoadedModule {
        &self.workspace.modules[self.module]
    }
}

/// The source-based identity of a definition inside a loaded workspace.
///
/// `def` in [`DefinitionId`] is a source-order position, so inserting a
/// definition changes the ids of everything after it. This key is independent
/// of that position: it fingerprints the definition's source and uses an
/// occurrence number only to distinguish same-name definitions. The numeric
/// id remains available for APIs that need to address the AST directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefinitionKey {
    pub module: usize,
    pub source_hash: u64,
    pub occurrence: usize,
}

/// The numeric source location of a definition in a loaded module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefinitionId {
    pub module: usize,
    pub def: usize,
}

/// One root definition that was considered by compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub id: DefinitionId,
    pub key: DefinitionKey,
    pub fingerprint: u64,
    pub name: String,
    pub result: Result<Rel, Diag>,
}

/// The complete result of checking and compiling a workspace's root module.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Compilation {
    pub queries: Vec<CompiledQuery>,
    pub diagnostics: Vec<Diag>,
}

impl Compilation {
    pub fn is_ok(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Check and compile every query in the root module.
pub fn compile(ws: &Workspace) -> Compilation {
    if let Some(cached) = ws.compilation_cache() {
        return cached;
    }
    let previous = ws.take_previous_compilation();
    let compilation = compile_uncached(ws, previous.as_ref());
    ws.set_compilation_cache(compilation.clone());
    compilation
}

/// Return diagnostics while avoiding a full clone of cached query trees.
pub fn compile_diagnostics(ws: &Workspace) -> Vec<Diag> {
    if let Some(diagnostics) = ws.compilation_diagnostics() {
        return diagnostics;
    }
    let previous = ws.take_previous_compilation();
    let compilation = compile_uncached(ws, previous.as_ref());
    let diagnostics = compilation.diagnostics.clone();
    ws.set_compilation_cache(compilation);
    diagnostics
}

fn compile_uncached(ws: &Workspace, previous: Option<&Compilation>) -> Compilation {
    let tc = crate::check::check(ws);
    compile_input_cached(CompilerInput::new(ws, &tc), previous)
}

/// Compile a workspace using a type check the caller already computed.
pub fn compile_checked(ws: &Workspace, tc: &TypeCheck) -> Compilation {
    compile_input(CompilerInput::new(ws, tc))
}

/// Compile an immutable view over cached workspace data.
pub fn compile_input(input: CompilerInput<'_>) -> Compilation {
    compile_input_cached(input, None)
}

fn compile_input_cached(input: CompilerInput<'_>, previous: Option<&Compilation>) -> Compilation {
    let tc = input.type_check();
    let root = input.root();
    let keys = definition_keys(root);
    let mut fingerprint_state = FingerprintState {
        key_cache: HashMap::from([(root.index(), keys.clone())]),
        cache: HashMap::new(),
        active: std::collections::HashSet::new(),
    };
    let mut out = Compilation {
        queries: Vec::new(),
        diagnostics: input.diagnostics().to_vec(),
    };
    for error in &tc.errors {
        push_unique(&mut out.diagnostics, error.diag.clone());
    }

    for (index, definition) in root.source().defs.iter().enumerate() {
        let name = definition.name.clone();
        let key = keys[index];
        let fingerprint =
            fingerprint_definition(input, tc, root.index(), index, key, &mut fingerprint_state);
        let id = DefinitionId {
            module: root.index(),
            def: index,
        };
        let result = previous
            .and_then(|old| {
                old.queries
                    .iter()
                    .find(|query| query.key == key && query.fingerprint == fingerprint)
                    .filter(|query| query.result.is_ok())
                    .map(|query| query.result.clone())
            })
            .or_else(
                || match elaborate_definition(input, root.index(), index, definition) {
                    Ok(query) => Some(match crate::checked::erase(query) {
                        Ok(rel) => match crate::schema::schema_located(&rel) {
                            Ok(_) => Ok(rel),
                            Err((location, message)) => Err(input.diagnostic(
                                location.map(|l| Origin::new(l.module, l.span)),
                                message,
                            )),
                        },
                        Err(error) => Err(internal_error(input, &name, &error)),
                    }),
                    Err(error) if is_not_a_query(&error) => {
                        tc.error_for(root.index(), index).cloned().map(Err)
                    }
                    Err(error) => Some(Err(match error.fault {
                        Fault::Program => input.diagnostic(error.origin, error.message),
                        Fault::Compiler => internal_error(input, &name, &error),
                        Fault::NotAQuery => unreachable!("handled above"),
                    })),
                },
            );
        let Some(result) = result else {
            continue;
        };
        if let Err(diag) = &result {
            push_unique(&mut out.diagnostics, diag.clone());
        }
        out.queries.push(CompiledQuery {
            id,
            key,
            fingerprint,
            name,
            result,
        });
    }
    out
}

#[derive(Default)]
struct StableHasher(u64);

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        };
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        self.0 = hash;
    }
}

fn stable_hash<T: Hash>(value: &T) -> u64 {
    let mut hasher = StableHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Build source-based identities before elaboration drops non-query
/// definitions from the result. Occurrences are counted by name, so editing a
/// definition's body does not renumber a later overload with the same name.
fn definition_keys(module: ModuleSnapshot<'_>) -> Vec<DefinitionKey> {
    let mut occurrences = HashMap::<&str, usize>::new();
    module
        .source()
        .defs
        .iter()
        .map(|definition| {
            let occurrence = occurrences.entry(definition.name.as_str()).or_insert(0);
            let source = module
                .text()
                .get(definition.span.start as usize..definition.span.end as usize)
                .unwrap_or_default();
            let key = DefinitionKey {
                module: module.index(),
                source_hash: stable_hash(&source),
                occurrence: *occurrence,
            };
            *occurrence += 1;
            key
        })
        .collect()
}

struct FingerprintState {
    key_cache: HashMap<usize, Vec<DefinitionKey>>,
    cache: HashMap<(usize, usize), u64>,
    active: std::collections::HashSet<(usize, usize)>,
}

fn fingerprint_definition(
    input: CompilerInput<'_>,
    tc: &TypeCheck,
    module: usize,
    def: usize,
    key: DefinitionKey,
    state: &mut FingerprintState,
) -> u64 {
    if let Some(&fingerprint) = state.cache.get(&(module, def)) {
        return fingerprint;
    }
    if !state.active.insert((module, def)) {
        // Cycles are rejected by elaboration; this marker only keeps the
        // fingerprint walk finite while a diagnostic is being built.
        return stable_hash(&(key, "cycle"));
    }
    let snapshot = input.module(module);
    let Some(definition) = snapshot.source().defs.get(def) else {
        return stable_hash(&(key, "missing"));
    };
    let choices: Vec<_> = tc
        .choices_of(module, def)
        .into_iter()
        .map(|(_, hole, choice)| format!("{hole}:{choice:?}"))
        .collect();
    let uses = expression_types(tc, module, &definition.body);
    let direct = (
        key,
        tc.type_of(module, def).unwrap_or_default().to_string(),
        tc.holes(module, def),
        format!("{:?}", tc.scheme_view(module, def)),
        format!("{:?}", tc.result_expr(module, def)),
        choices,
        uses,
    );
    let mut dependencies = definition_dependencies(input, module, &definition.body);
    dependencies.sort_unstable();
    dependencies.dedup();
    let dependency_fingerprints: Vec<_> = dependencies
        .into_iter()
        .map(|(dependency_module, dependency_def)| {
            let dependency_key = state
                .key_cache
                .entry(dependency_module)
                .or_insert_with(|| definition_keys(input.module(dependency_module)))
                [dependency_def];
            fingerprint_definition(
                input,
                tc,
                dependency_module,
                dependency_def,
                dependency_key,
                state,
            )
        })
        .collect();
    state.active.remove(&(module, def));
    let fingerprint = stable_hash(&(direct, dependency_fingerprints));
    state.cache.insert((module, def), fingerprint);
    fingerprint
}

fn expression_types(
    tc: &TypeCheck,
    module: usize,
    expression: &cagara_syntax::ast::Expr,
) -> Vec<String> {
    let mut out = Vec::new();
    walk_expression(expression, &mut |e| {
        if let Some(ty) = tc.use_ty(module, e.id) {
            out.push(format!("{ty:?}"));
        }
    });
    out
}

fn definition_dependencies(
    input: CompilerInput<'_>,
    module: usize,
    expression: &cagara_syntax::ast::Expr,
) -> Vec<(usize, usize)> {
    let mut dependencies = Vec::new();
    walk_expression(expression, &mut |e| {
        let snapshot = input.module(module);
        match &e.kind {
            cagara_syntax::ast::ExprKind::Name(name) => {
                if let Some(binding) = snapshot.scope().get(name) {
                    collect_binding_dependencies(binding, &mut dependencies);
                }
            }
            cagara_syntax::ast::ExprKind::Proj(base, field) => {
                if let cagara_syntax::ast::ExprKind::Name(alias) = &base.kind {
                    if let Some(Binding::Module(target)) = snapshot.scope().get(alias) {
                        if let Some(binding) = input.module(*target).own().get(field) {
                            collect_binding_dependencies(binding, &mut dependencies);
                        }
                    }
                }
            }
            _ => {}
        }
    });
    dependencies
}

fn collect_binding_dependencies(binding: &Binding, dependencies: &mut Vec<(usize, usize)>) {
    match binding {
        Binding::Def(module, def) => dependencies.push((*module, *def)),
        Binding::Overloads(module, defs) => {
            dependencies.extend(defs.iter().map(|def| (*module, *def)))
        }
        Binding::Module(_) | Binding::Prim(_) => {}
    }
}

fn walk_expression(
    expression: &cagara_syntax::ast::Expr,
    visit: &mut impl FnMut(&cagara_syntax::ast::Expr),
) {
    visit(expression);
    match &expression.kind {
        cagara_syntax::ast::ExprKind::Proj(base, _)
        | cagara_syntax::ast::ExprKind::Lambda(_, base) => walk_expression(base, visit),
        cagara_syntax::ast::ExprKind::App(function, args) => {
            walk_expression(function, visit);
            for arg in args.iter() {
                walk_expression(arg, visit);
            }
        }
        cagara_syntax::ast::ExprKind::Record(fields) => {
            for (_, value) in fields {
                walk_expression(value, visit);
            }
        }
        cagara_syntax::ast::ExprKind::List(values) => {
            for value in values {
                walk_expression(value, visit);
            }
        }
        cagara_syntax::ast::ExprKind::Name(_)
        | cagara_syntax::ast::ExprKind::Lit(_)
        | cagara_syntax::ast::ExprKind::Field(_, _)
        | cagara_syntax::ast::ExprKind::Sql(_)
        | cagara_syntax::ast::ExprKind::Primitive(_)
        | cagara_syntax::ast::ExprKind::Error => {}
    }
}

/// Elaborate root definitions directly into checked queries and erase them.
///
/// Type checking supplies the facts used by source elaboration. A checked
/// query is the sole source of a production `Rel`.
pub fn root_queries_checked(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<Rel, Diag>)> {
    compile_input(CompilerInput::new(ws, tc))
        .queries
        .into_iter()
        .map(|query| (query.name, query.result))
        .collect()
}

/// Check and compile every query in the root module.
pub fn root_queries(ws: &Workspace) -> Vec<(String, Result<Rel, Diag>)> {
    compile_input(CompilerInput::new(ws, &crate::check::check(ws)))
        .queries
        .into_iter()
        .map(|query| (query.name, query.result))
        .collect()
}

fn push_unique(diags: &mut Vec<Diag>, diag: Diag) {
    if !diags.contains(&diag) {
        diags.push(diag);
    }
}

fn internal_error(input: CompilerInput<'_>, name: &str, error: &Error) -> Diag {
    let detail = error.message.clone();
    let message = format!("internal error in `{name}`: {detail}");
    input.diagnostic(error.origin, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overloaded_query_definitions_keep_source_identity() {
        let ws = Workspace::from_source(
            "q : query { a = int } = table \"s\" \"one\"\n\
             q : query { a = int } = table \"s\" \"two\"\n",
        );
        let tc = crate::check::check(&ws);
        let compilation = compile_checked(&ws, &tc);
        assert!(compilation.diagnostics.is_empty());
        let tables: Vec<_> = compilation
            .queries
            .into_iter()
            .map(|query| match query.result.unwrap() {
                Rel::At(_, rel) => match *rel {
                    Rel::Table { name, .. } => name,
                    other => panic!("expected table, got {other:?}"),
                },
                other => panic!("expected located table, got {other:?}"),
            })
            .collect();
        assert_eq!(tables, vec!["one", "two"]);

        let compilation = compile(&ws);
        assert_eq!(
            compilation
                .queries
                .iter()
                .map(|query| query.id.def)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_ne!(
            compilation.queries[0].key, compilation.queries[1].key,
            "duplicate definitions need distinct cache identities"
        );
    }

    #[test]
    fn definition_keys_survive_unrelated_insertions() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );
        let before = compile(&ws);
        let keys_before: Vec<_> = before.queries.iter().map(|query| query.key).collect();

        assert!(ws.set_source(
            ws.root,
            "inserted : query { a = int } = table \"public\" \"inserted\"\n\
             first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let after = compile(&ws);
        let by_name: HashMap<_, _> = after
            .queries
            .iter()
            .map(|query| (query.name.as_str(), query.key))
            .collect();

        assert_eq!(by_name["first"], keys_before[0]);
        assert_eq!(by_name["second"], keys_before[1]);
    }

    #[test]
    fn inserting_a_definition_reuses_unchanged_queries() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );
        compile(&ws);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "inserted : query { a = int } = table \"public\" \"inserted\"\n\
             first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let after = compile(&ws);

        assert!(after.diagnostics.is_empty(), "{:?}", after.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0)],
            "only the inserted definition should be elaborated"
        );
    }

    #[test]
    fn changing_a_dependency_reelaborates_its_users() {
        let mut ws = Workspace::from_source(
            "base : query { a = int } = table \"public\" \"base\"\n\
             q = base\n",
        );
        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "base : query { a = int } = table \"public\" \"changed\"\n\
             q = base\n"
                .into()
        ));
        let edited = compile(&ws);

        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0), (ws.root, 1)],
            "a dependent definition must not reuse a stale expanded query"
        );
    }

    #[test]
    #[ignore = "large-definition benchmark; set CAGARA_BENCH_DEFINITIONS to scale it"]
    fn large_definition_cache_benchmark() {
        let count = std::env::var("CAGARA_BENCH_DEFINITIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100_000);
        let mut source = String::with_capacity(count.saturating_mul(64));
        for i in 0..count {
            source.push_str(&format!(
                "d{i} : query {{ a = int }} = table \"public\" \"t{i}\"\n"
            ));
        }
        let mut ws = Workspace::from_source(&source);
        assert!(ws.diags.is_empty(), "load diagnostics: {:?}", ws.diags);

        let started = std::time::Instant::now();
        let first = compile(&ws);
        let first_elapsed = started.elapsed();
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);

        let edit = format!(
            "d0 : query {{ a = int }} = table \"public\" \"changed\"\n{}",
            source
                .lines()
                .skip(1)
                .map(|line| format!("{line}\n"))
                .collect::<String>()
        );
        let before_edit = crate::elaborate::elaborated_defs().len();
        assert!(ws.set_source(ws.root, edit));
        let started = std::time::Instant::now();
        let second = compile(&ws);
        let second_elapsed = started.elapsed();
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        assert_eq!(
            crate::elaborate::elaborated_defs().len() - before_edit,
            1,
            "one-definition edits should elaborate one query"
        );
        println!("{count} definitions: initial {first_elapsed:?}, one-edit {second_elapsed:?}");
    }

    #[test]
    fn editing_one_definition_changes_only_its_key() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );
        let before = compile(&ws);
        let first_before = before.queries[0].key;
        let second_before = before.queries[1].key;

        assert!(ws.set_source(
            ws.root,
            "first : query { a = int } = table \"public\" \"changed\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let after = compile(&ws);

        assert_ne!(after.queries[0].key, first_before);
        assert_eq!(after.queries[1].key, second_before);
    }

    #[test]
    fn compiler_input_is_a_read_only_view_of_the_workspace() {
        let ws = Workspace::from_source("q = 1\n");
        let tc = crate::check::check(&ws);
        let input = CompilerInput::new(&ws, &tc);
        let root = input.root();

        assert_eq!(root.index(), ws.root);
        assert_eq!(root.path(), Path::new("<input>"));
        assert_eq!(root.text(), "q = 1\n");
        assert_eq!(root.source().defs[0].name, "q");
        assert!(root.scope().contains_key("q"));

        assert_eq!(compile_input(input), compile_checked(&ws, &tc));
    }

    #[test]
    fn compiler_input_preserves_source_diagnostics() {
        let ws = Workspace::from_source("q = (\n");
        let tc = crate::check::check(&ws);
        let compilation = compile_input(CompilerInput::new(&ws, &tc));

        assert!(!compilation.diagnostics.is_empty());
        assert_eq!(compilation.diagnostics, ws.diags);
    }

    #[test]
    fn diagnostics_reuse_the_cached_compilation() {
        let ws = Workspace::from_source("q = (\n");

        let first = compile_diagnostics(&ws);
        let after_first = crate::elaborate::elaborate_runs();
        let second = compile_diagnostics(&ws);

        assert_eq!(second, first);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first,
            "cached diagnostics should not re-elaborate the source"
        );
    }

    #[test]
    fn compilation_cache_reuses_and_invalidates_elaboration() {
        let mut ws = Workspace::from_source("q : query { a = int } = table \"public\" \"items\"\n");

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let after_first = crate::elaborate::elaborate_runs();

        let second = compile(&ws);
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first,
            "unchanged compilation should reuse the cached result"
        );

        assert!(ws.set_source(
            ws.root,
            "q : query { a = int } = table \"public\" \"updated\"\n".into()
        ));
        let edited = compile(&ws);
        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first + 1,
            "an accepted edit should invalidate the cached compilation"
        );
    }

    #[test]
    fn changing_the_compilation_root_invalidates_the_cache() {
        let root_path = std::path::PathBuf::from("/tmp/cagara-root-cache/root.cagara");
        let lib_path = std::path::PathBuf::from("/tmp/cagara-root-cache/lib.cagara");
        let root_src = "import \"lib.cagara\" as lib\nq = lib.value\n";
        let lib_src = "value : query { a = int } = table \"public\" \"items\"\n";
        let mut buffers = std::collections::HashMap::new();
        buffers.insert(root_path.clone(), root_src.to_string());
        buffers.insert(lib_path.clone(), lib_src.to_string());
        let mut ws = Workspace::open_with_buffers(&root_path, root_src.to_string(), &buffers);

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let after_first = crate::elaborate::elaborate_runs();

        assert!(ws.set_root_path(&lib_path));
        let second = compile(&ws);
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        assert_eq!(
            crate::elaborate::elaborate_runs(),
            after_first + 1,
            "changing roots must not reuse the previous root compilation"
        );
    }

    #[test]
    fn editing_one_definition_reuses_unchanged_root_definitions() {
        let mut ws = Workspace::from_source(
            "first : query { a = int } = table \"public\" \"first\"\n\
             second : query { a = int } = table \"public\" \"second\"\n",
        );

        let first = compile(&ws);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let before = crate::elaborate::elaborated_defs().len();

        assert!(ws.set_source(
            ws.root,
            "first : query { a = int } = table \"public\" \"changed\"\n\
             second : query { a = int } = table \"public\" \"second\"\n"
                .into()
        ));
        let edited = compile(&ws);
        assert!(edited.diagnostics.is_empty(), "{:?}", edited.diagnostics);
        assert_eq!(
            &crate::elaborate::elaborated_defs()[before..],
            &[(ws.root, 0)],
            "an unchanged definition should reuse its erased query"
        );
    }
}
